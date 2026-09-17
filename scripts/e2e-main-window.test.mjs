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
