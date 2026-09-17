import test from "node:test";
import assert from "node:assert/strict";
import { chooseMainWindow, isAuxiliaryWindow } from "./lib/e2e-main-window.mjs";

const APP = "http://tauri.localhost/";
const CAPTURE = "http://tauri.localhost/capture.html";

test("recognises Tine's auxiliary documents", () => {
  assert.equal(isAuxiliaryWindow(CAPTURE), true);
  assert.equal(isAuxiliaryWindow("http://tauri.localhost/capture.html?graph=x"), true);
  assert.equal(isAuxiliaryWindow(APP), false);
  assert.equal(isAuxiliaryWindow("http://tauri.localhost/index.html"), false);
});

test("switches away from the window windows-smoke was stranded on", () => {
  const windows = [{ handle: "a", url: CAPTURE }, { handle: "b", url: APP }];
  assert.equal(chooseMainWindow(windows, "a"), "b");
});

test("stays put when the session is already on the app", () => {
  const windows = [{ handle: "a", url: CAPTURE }, { handle: "b", url: APP }];
  assert.equal(chooseMainWindow(windows, "b"), null);
});

test("reports that there is no app window at all, rather than picking one", () => {
  assert.equal(chooseMainWindow([{ handle: "a", url: CAPTURE }], "a"), undefined);
});

// The breadth half of the fix. The first repair put `ensureMainWindow` in
// `e2e-pdf-logseq.mjs` alone, because that was the journey whose failure named
// the capture window. A later burn-in then failed three OTHER journeys on the
// same runner batch -- `page-properties` ("Quick Switcher did not open ...
// url: capture.html"), `print-security` ("the application search control never
// became clickable: missing") and `windows-core` ("shell, scroll pane and
// seeded block did not become ready") -- one cause wearing three costumes. So
// the requirement belongs to the whole suite, not to the journey that happened
// to report it first.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptsDir = path.dirname(fileURLToPath(import.meta.url));

/** The journey scripts the windows-smoke suite runs. */
export function windowsSmokeScripts(runnerSource) {
  const start = runnerSource.indexOf('"windows-smoke": [');
  if (start < 0) throw new Error("run-e2e.mjs no longer declares a windows-smoke suite");
  const end = runnerSource.indexOf("\n  ],", start);
  return [...new Set([...runnerSource.slice(start, end).matchAll(/"(scripts\/e2e-[^"]+\.mjs)"/g)]
    .map((match) => match[1]))].sort();
}

test("every windows-smoke journey points its session at the app window", () => {
  const runner = fs.readFileSync(path.join(scriptsDir, "run-e2e.mjs"), "utf8");
  const journeys = windowsSmokeScripts(runner);
  assert.ok(journeys.length >= 6, `expected the windows-smoke suite, saw ${journeys.length} journeys`);
  const missing = journeys.filter((journey) => {
    const source = fs.readFileSync(path.join(scriptsDir, "..", journey), "utf8");
    return !/ensureMainWindow\s*\(/.test(source);
  });
  assert.deepEqual(
    missing,
    [],
    "These windows-smoke journeys create a WebDriver session without calling ensureMainWindow(browser).\n"
    + "On Windows the driver can attach to the Quick Capture window, where every application\n"
    + "selector is legitimately absent, and the journey then reports its own missing element\n"
    + "rather than the wrong window. scripts/e2e-print-security.mjs is the exemplar call site.",
  );
});
