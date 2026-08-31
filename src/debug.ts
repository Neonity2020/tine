// Frontend half of the startup debug trace (see main.rs → "Startup debug
// logging"). When the backend reports debug mode on (TINE_DEBUG=1 / --debug), we
// forward the webview's own milestones and uncaught errors into the SAME backend
// log file — so a "the window didn't load" report is captured end-to-end (Rust
// startup + did-the-frontend-boot + any JS error) in one file the user sends back.

import { backend } from "./backend";
import { openSettings, pushToast } from "./ui";
import { platformKind } from "./nativeChrome";

let enabled = false;
let initialized = false;

/** Append a line to the backend debug log (no-op unless debug mode is on). */
export function dbg(line: string): void {
  if (enabled) void backend().debugLog(line).catch(() => {});
}

/** Probe debug mode, and if on: forward errors, log that the frontend booted, and
 *  tell the user where the log lives. Fire-and-forget; never throws. */
export async function initDebug(): Promise<void> {
  if (initialized) return;
  initialized = true;
  let info: { enabled: boolean; path: string; recorderActive: boolean; previousExitUnclean: boolean };
  try {
    info = await backend().debugInfo();
  } catch {
    return; // browser mock / command missing — nothing to do
  }
  // The always-on report records only a fixed kind and numeric coordinates.
  // The opt-in trace below may include the actual message and filename.
  window.addEventListener("error", (e) => {
    void backend().diagnosticFrontendEvent("uncaught_error", e.lineno || undefined, e.colno || undefined).catch(() => {});
    dbg(`window.onerror: ${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`);
  });
  window.addEventListener("unhandledrejection", (e) => {
    void backend().diagnosticFrontendEvent("unhandled_rejection").catch(() => {});
    dbg(`unhandledrejection: ${String((e as PromiseRejectionEvent).reason)}`);
  });

  const heartbeatMs = 2_000;
  let expected = performance.now() + heartbeatMs;
  window.setInterval(() => {
    const now = performance.now();
    const delay = now - expected;
    expected = now + heartbeatMs;
    if (delay >= 5_000) {
      void backend().diagnosticFrontendEvent("heartbeat_delay", undefined, undefined, Math.round(delay)).catch(() => {});
    }
  }, heartbeatMs);

  if (info.previousExitUnclean) {
    pushToast("Tine did not close cleanly last time. A privacy-safe diagnostic report is available.", "warn", {
      sticky: true,
      action: { label: "Diagnostics", run: () => openSettings("diagnostics") },
    });
  }

  if (!info.enabled) return;
  enabled = true;

  // platform= is the identity Rust injected, which is NOT derivable from ua=
  // on iPadOS (GH #446). Keeping both in one line makes that divergence
  // readable in a bug report and assertable from the iOS probe.
  dbg(`frontend booted (platform=${platformKind} ua=${navigator.userAgent})`);
  pushToast(`Debug logging is ON → ${info.path}`, "info");
}
