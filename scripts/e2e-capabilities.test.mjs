import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";

import {
  createWebdriverLifecycle,
  findTaggedWebdriverProcesses,
  parseFrameExtentsOutput,
  tauriCapabilities,
  waitForHttpServer,
  webdriverSessionToken,
  webdriverServerArgs,
} from "./e2e-capabilities.mjs";
import fs from "node:fs";
import os from "node:os";

// DUP-12 (2026-08-25 duplication audit): these cases pin the domains where the
// previously per-suite harness spellings disagreed — capability objects,
// launch args, server readiness waits, and _NET_FRAME_EXTENTS parsing.

test("Linux capability object equals the historical inline wry literal", () => {
  assert.deepEqual(tauriCapabilities("/opt/tine/tine", "default", "linux"), {
    browserName: "wry",
    "wdio:enforceWebDriverClassic": true,
    "tauri:options": { application: "/opt/tine/tine" },
  });
});

test("Windows capability requires the WebView2 user-data root", () => {
  assert.throws(
    () => tauriCapabilities("C:\\tine.exe", "default", "win32"),
    /E2E_WEBVIEW_USER_DATA_ROOT/,
  );
});

test("Windows capability uses the documented EdgeDriver attach + launch modes", () => {
  const env = { E2E_WEBVIEW_USER_DATA_ROOT: "C:\\tmp\\e2e-root" };
  const orig = process.env.E2E_WEBVIEW_USER_DATA_ROOT;
  process.env.E2E_WEBVIEW_USER_DATA_ROOT = env.E2E_WEBVIEW_USER_DATA_ROOT;
  try {
    assert.deepEqual(tauriCapabilities("C:\\tine.exe", "my suite", "win32"), {
      browserName: "webview2",
      "wdio:enforceWebDriverClassic": true,
      "ms:edgeChromium": true,
      "ms:edgeOptions": {
        binary: "C:\\tine.exe",
        args: [],
        webviewOptions: { userDataFolder: path.join("C:\\tmp\\e2e-root", "my-suite") },
      },
    });
    // A fixed remote-debugging port switches to attach mode (no binary launch).
    assert.deepEqual(
      tauriCapabilities("C:\\tine.exe", "my suite", "win32", "127.0.0.1:4567"),
      {
        browserName: "webview2",
        "wdio:enforceWebDriverClassic": true,
        "ms:edgeChromium": true,
        "ms:edgeOptions": { debuggerAddress: "127.0.0.1:4567" },
      },
    );
  } finally {
    if (orig === undefined) delete process.env.E2E_WEBVIEW_USER_DATA_ROOT;
    else process.env.E2E_WEBVIEW_USER_DATA_ROOT = orig;
  }
});

test("launch args equal the historical full spelling on Linux, --port only on Windows", () => {
  assert.deepEqual(webdriverServerArgs(4444, 4445, "/usr/bin/WebKitWebDriver", "linux"), [
    "--port",
    "4444",
    "--native-port",
    "4445",
    "--native-driver",
    "/usr/bin/WebKitWebDriver",
  ]);
  assert.deepEqual(webdriverServerArgs(4444, 4445, "C:\\WebKitWebDriver.exe", "win32"), [
    "--port=4444",
  ]);
});

test("frame extents: parse a normal xprop reply identically on both policies", () => {
  const raw = "_NET_FRAME_EXTENTS(CARDINAL) = 1, 2, 3, 4\n";
  const want = { left: 1, right: 2, top: 3, bottom: 4 };
  assert.deepEqual(parseFrameExtentsOutput(raw), want);
  assert.deepEqual(parseFrameExtentsOutput(raw, { strict: true }), want);
});

test("frame extents: the previously divergent 'not found' case is one explicit choice", () => {
  const raw = "_NET_FRAME_EXTENTS: not found.\n";
  // The lax policy treats a missing property as an undecorated window.
  assert.deepEqual(parseFrameExtentsOutput(raw), { left: 0, right: 0, top: 0, bottom: 0 });
  // The strict policy treats it as evidence the window manager is not ready.
  assert.throws(() => parseFrameExtentsOutput(raw, { strict: true }), /malformed frame extents/);
});

test("frame extents: garbage output throws under both policies", () => {
  assert.throws(() => parseFrameExtentsOutput("utter nonsense"), /malformed frame extents/);
  assert.throws(() => parseFrameExtentsOutput("utter nonsense", { strict: true }), /malformed/);
});

test("waitForHttpServer: resolves on the first healthy response", async () => {
  let calls = 0;
  await waitForHttpServer("http://127.0.0.1:9/", 5, 1, async () => {
    calls += 1;
    return { ok: true };
  });
  assert.equal(calls, 1);
});

test("waitForHttpServer: retries through failures and reports the attempt budget", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    if (calls < 3) throw new Error("ECONNREFUSED");
    return { ok: true };
  };
  await waitForHttpServer("http://127.0.0.1:9/", 5, 1, fetchImpl);
  assert.equal(calls, 3);

  calls = 0;
  await assert.rejects(
    () => waitForHttpServer("http://127.0.0.1:9/", 2, 1, fetchImpl),
    /did not start at http:\/\/127\.0\.0\.1:9\/ after 2 attempts/,
  );
  assert.equal(calls, 2);
});

test("webdriver lifecycle finds only the exact tagged process tree", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tine-e2e-proc-"));
  const token = webdriverSessionToken("managed feed", 4724, 4725);
  for (const [pid, environment] of [
    ["101", `A=1\0TINE_E2E_WEBDRIVER_SESSION=${token}\0`],
    ["102", `TINE_E2E_WEBDRIVER_SESSION=${token}-other\0`],
    ["103", "A=1\0"],
  ]) {
    fs.mkdirSync(path.join(root, pid));
    fs.writeFileSync(path.join(root, pid, "environ"), environment);
  }
  assert.deepEqual(findTaggedWebdriverProcesses(token, root), [101]);
});

test("webdriver lifecycle deadline reaps its tagged session and records why", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tine-e2e-proc-"));
  const token = webdriverSessionToken("deadline", 4824, 4825);
  fs.mkdirSync(path.join(root, "201"));
  fs.writeFileSync(
    path.join(root, "201", "environ"),
    `TINE_E2E_WEBDRIVER_SESSION=${token}\0`,
  );
  const signals = [];
  const lifecycle = createWebdriverLifecycle({
    scenario: "deadline",
    driverPort: 4824,
    nativePort: 4825,
    callTimeoutMs: 5,
    cleanupGraceMs: 0,
    platform: "linux",
    procRoot: root,
    kill: (pid, signal) => {
      signals.push([pid, signal]);
      if (signal === "SIGKILL") fs.rmSync(path.join(root, String(pid)), { recursive: true });
    },
  });
  await assert.rejects(
    lifecycle.run("stalled screenshot", () => new Promise(() => {})),
    /stalled screenshot.*exceeded 5 ms.*reaped/,
  );
  assert.deepEqual(signals, [[201, "SIGTERM"], [201, "SIGKILL"]]);
  assert.deepEqual(lifecycle.evidence.timeouts, [{ label: "stalled screenshot", timeoutMs: 5 }]);
  assert.deepEqual(lifecycle.evidence.reaped[0].survivors, []);
});

test("webdriver lifecycle never signals an unverified stored driver process group", async () => {
  const signals = [];
  const lifecycle = createWebdriverLifecycle({
    scenario: "pid-reuse",
    driverPort: 4924,
    nativePort: 4925,
    cleanupGraceMs: 0,
    platform: "linux",
    procRoot: fs.mkdtempSync(path.join(os.tmpdir(), "tine-e2e-proc-")),
    kill: (pid, signal) => signals.push([pid, signal]),
  });
  await lifecycle.stop({ driver: { pid: 301 }, label: "already-exited-driver" });
  assert.deepEqual(signals, []);
});

test("webdriver lifecycle fails closed with exact-token survivor evidence", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tine-e2e-proc-"));
  const token = webdriverSessionToken("unkillable", 5024, 5025);
  fs.mkdirSync(path.join(root, "401"));
  fs.writeFileSync(
    path.join(root, "401", "environ"),
    `TINE_E2E_WEBDRIVER_SESSION=${token}\0`,
  );
  const lifecycle = createWebdriverLifecycle({
    scenario: "unkillable",
    driverPort: 5024,
    nativePort: 5025,
    cleanupGraceMs: 0,
    survivorTimeoutMs: 5,
    survivorPollMs: 1,
    platform: "linux",
    procRoot: root,
    kill: () => true,
  });
  await assert.rejects(
    lifecycle.reap("test-survivor", { graceMs: 0 }),
    /left exact-token survivors: 401/,
  );
  assert.deepEqual(lifecycle.evidence.reaped[0].survivors, [401]);
});
