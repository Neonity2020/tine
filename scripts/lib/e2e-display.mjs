// Native E2E scenarios drive the real Tauri app through tauri-driver +
// WebKitWebDriver, so they need an X display. With no DISPLAY the app dies
// inside the driver with "Failed to initialize gtk backend!", and the scenario
// only ever surfaces `UND_ERR_HEADERS_TIMEOUT` from the WebDriver session POST
// — which reads as a hang, not as a missing display. That misdiagnosis cost two
// debugging cycles on 2026-08-09.
//
// `scripts/run-e2e.mjs` already wraps every native Linux scenario in
// `xvfb-run -a`, so a suite run is unaffected: DISPLAY is set and this helper
// returns immediately. This exists for the other half of the workflow — running
// one `scripts/e2e-*.mjs` directly — where it either provisions a display or
// fails fast naming the remedy.

import { spawn } from "node:child_process";
import fs from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const CANDIDATE_DISPLAYS = [":98", ":99", ":100", ":101"];

let provisioned;

function remediation(detail) {
  return new Error(
    `${detail}\n` +
      "A native E2E scenario needs an X display. Either run it under a display " +
      "(`xvfb-run -a node scripts/e2e-<name>.mjs`), or use the suite runner " +
      "(`npm run e2e:linux:smoke` / `e2e:linux:release`), which wraps every " +
      "native Linux scenario in `xvfb-run -a` itself.",
  );
}

/** Terminate an Xvfb this module started. Safe to call when it started none. */
export function stopDisplay() {
  if (!provisioned) return;
  const victim = provisioned;
  provisioned = undefined;
  try {
    victim.kill("SIGKILL");
  } catch {}
}

/**
 * Guarantee `process.env.DISPLAY` names a usable X display, starting a private
 * Xvfb when one is not inherited. No-op off Linux and no-op when the caller
 * already has a display (including under `xvfb-run`).
 */
export async function ensureDisplay({ geometry = "1400x1000x24" } = {}) {
  if (process.env.DISPLAY) return process.env.DISPLAY;
  if (process.platform !== "linux") return undefined;

  const displays = process.env.XVFB_DISPLAY ? [process.env.XVFB_DISPLAY] : CANDIDATE_DISPLAYS;
  let lastError = "";
  let lastLog = "";
  for (const display of displays) {
    const suffix = display.replace(/[^0-9]/g, "") || "x";
    lastLog = `/tmp/xvfb-e2e-${suffix}.log`;
    let log;
    try {
      log = fs.openSync(lastLog, "w");
    } catch (error) {
      lastError = `could not open ${lastLog}: ${error.message}`;
      continue;
    }
    let spawnError = "";
    const child = spawn("Xvfb", [display, "-screen", "0", geometry], {
      stdio: ["ignore", log, log],
    });
    child.on("error", (error) => {
      spawnError = error.message;
    });
    await sleep(900);
    if (spawnError) {
      lastError = spawnError;
      continue;
    }
    if (child.exitCode !== null) {
      lastError = `display ${display} exited with code ${child.exitCode}`;
      continue;
    }
    provisioned = child;
    child.unref();
    process.env.DISPLAY = display;
    process.once("exit", stopDisplay);
    process.once("SIGINT", () => {
      stopDisplay();
      process.exit(130);
    });
    process.once("SIGTERM", () => {
      stopDisplay();
      process.exit(143);
    });
    return display;
  }
  throw remediation(
    lastError
      ? `no DISPLAY, and Xvfb could not be started (${lastError}; see ${lastLog}).`
      : "no DISPLAY, and no Xvfb display could be started.",
  );
}
