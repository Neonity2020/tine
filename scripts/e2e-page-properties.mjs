// Real Tauri/WebKit and disk round-trip for GH #163. The literal reporter
// samples travel through page load -> page actions -> properties panel -> reactive store -> debounced
// native save; helper-only string tests cannot prove this boundary.
import { spawn, spawnSync } from "node:child_process";
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
import { waitForFileText } from "./e2e-file-poll.mjs";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { openPageByName } from "./lib/e2e-navigation.mjs";

await ensureDisplay();

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = process.env.TINE_APP || path.join(ROOT, process.platform === "win32" ? "target/release/tine.exe" : "target/release/tine");
const TD = process.env.TAURI_DRIVER || "tauri-driver";
const DRIVER = Number(process.env.E2E_DRIVER_PORT || 4592);
const NATIVE = Number(process.env.E2E_NATIVE_PORT || 4593);
const TMP = path.join(os.tmpdir(), `tine-page-properties-e2e-${process.pid}`);
const GRAPH = path.join(TMP, "graph");

fs.rmSync(TMP, { recursive: true, force: true });
for (const dir of ["pages", "journals", "logseq", "assets"]) fs.mkdirSync(`${GRAPH}/${dir}`, { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${TMP}/xdg/${dir}`, { recursive: true });
fs.writeFileSync(`${GRAPH}/logseq/config.edn`, "{}\n");
const detailed = [
  "alias:: Test Record",
  "ai-prompt:: [[Prompt-Test]]",
  "usage-frequency:: [[Frequency-High]]",
  "",
  "page-level:: [[Level-Two]]",
  "layout:: [[Layout-Top-Collapsed]]",
  "component-state:: [[Component-Wide]]",
  "",
  "timestamp:: 20250707092601",
  "observation-target:: [[Object-Test-Page]]",
  "external-impact::",
  "--:: --",
  "methods:: [[Method-A]] [[Method-B]]",
  "key-conclusion:: [[Conclusion-A]] [[Conclusion-B]]",
  "",
  "- Example content block",
  "",
].join("\n");
const simple = "A:: XX\r\nB:: XX\r\nC:: XX\r\n";
const deletion = "alias:: Delete Me\ncustom/key:: transient\n\n- Body survives\n";
fs.writeFileSync(`${GRAPH}/pages/Property detailed.md`, detailed);
fs.writeFileSync(`${GRAPH}/pages/Property simple.md`, simple);
fs.writeFileSync(`${GRAPH}/pages/Property deletion.md`, deletion);
const now = new Date();
const journal = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
fs.writeFileSync(`${GRAPH}/journals/${journal}.md`, "- [[Property detailed]]\n- [[Property simple]]\n- [[Property deletion]]\n");

const env = {
  ...process.env,
  TINE_GRAPH: GRAPH,
  XDG_DATA_HOME: `${TMP}/xdg/data`,
  XDG_CONFIG_HOME: `${TMP}/xdg/config`,
  XDG_CACHE_HOME: `${TMP}/xdg/cache`,
  APPDATA: path.join(TMP, "appdata"),
  LOCALAPPDATA: path.join(TMP, "localappdata"),
  WEBKIT_DISABLE_DMABUF_RENDERER: "1",
  WEBKIT_DISABLE_COMPOSITING_MODE: "1",
  LIBGL_ALWAYS_SOFTWARE: "1",
  GDK_BACKEND: "x11",
};

if (process.platform === "win32" && process.env.CI === "true") {
  spawnSync("taskkill", ["/IM", path.basename(APP), "/T", "/F"], { stdio: "ignore" });
}
const webviewTarget = await startWebdriverApplication(APP, env, NATIVE);
const log = fs.openSync(path.join(process.env.E2E_ARTIFACT_DIR || TMP, "tauri-driver.log"), "w");
const driverArgs = webdriverServerArgs(
  DRIVER,
  NATIVE,
  process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver",
);
const driver = spawn(TD, driverArgs, {
  env: webviewTarget.env,
  stdio: ["ignore", log, log],
  detached: process.platform !== "win32",
});
await sleep(2500);
let browser;

// One shared readiness contract for opening a page: find and activate the
// exact non-block row in a single round trip and retry against the routed
// title, so no element handle outlives the switcher's re-render. See
// scripts/lib/e2e-navigation.mjs.
const openPage = (name) => openPageByName(browser, name);

async function openPageProperties() {
  await browser.$("[data-page-actions-trigger]").click();
  const item = await browser.$('[data-page-action-id="page-properties"]');
  await item.waitForExist({ timeout: 5_000 });
  await item.click();
  await browser.$(".page-props-panel").waitForExist({ timeout: 5_000 });
}

async function setPagePropertyField(label, value) {
  await openPageProperties();
  const index = await browser.execute((wanted) => {
    const fields = [...document.querySelectorAll(".page-props-panel .pp-field")];
    return fields.findIndex((field) => field.querySelector(".pp-label")?.textContent?.trim() === wanted);
  }, label);
  if (index < 0) throw new Error(`missing page property field ${label}`);
  const input = (await browser.$$(".page-props-panel .pp-field .pp-input"))[index];
  await input.setValue(value);
  await browser.keys("Enter");
  await browser.$(".page-props-panel").waitForExist({ reverse: true, timeout: 5_000 });
}

async function waitForFile(file, predicate, label) {
  return waitForFileText(file, predicate, label);
}

async function nativeTab({ shift = false } = {}) {
  const actions = shift
    ? [{ type: "keyDown", value: "\uE008" }, { type: "keyDown", value: "\uE004" }, { type: "keyUp", value: "\uE004" }, { type: "keyUp", value: "\uE008" }]
    : [{ type: "keyDown", value: "\uE004" }, { type: "keyUp", value: "\uE004" }];
  await browser.performActions([{ type: "key", id: "page-properties-keyboard", actions }]);
  await browser.releaseActions();
}

async function nativeSelectAll() {
  await browser.performActions([{
    type: "key",
    id: "page-properties-select-all",
    actions: [
      { type: "keyDown", value: "\uE009" },
      { type: "keyDown", value: "a" },
      { type: "keyUp", value: "a" },
      { type: "keyUp", value: "\uE009" },
    ],
  }]);
  await browser.releaseActions();
}

async function nativeArrowDown() {
  // The form traversal above uses a modifier-bearing W3C source. Reset every
  // remote input source before this semantic editor action: a leaked Shift
  // makes the editor correctly leave native selection navigation untouched.
  await browser.releaseActions();
  await browser.performActions([{
    type: "key",
    id: "page-properties-arrow-down",
    actions: [
      { type: "keyDown", value: "\uE015" },
      { type: "keyUp", value: "\uE015" },
    ],
  }]);
  await browser.releaseActions();
}

async function pageArrowDownCapsule(phase) {
  return browser.execute((failurePhase) => {
    const describeEditor = (element) => {
      if (!(element instanceof HTMLTextAreaElement)) return null;
      const block = element.closest("[data-block-id]");
      const surface = element.closest("[data-pane-id], [data-sidebar-surface], [data-surface-id]");
      return {
        value: element.value,
        selection: [element.selectionStart, element.selectionEnd],
        blockId: block?.getAttribute("data-block-id") ?? null,
        surfaceId: surface?.getAttribute("data-pane-id")
          ?? surface?.getAttribute("data-sidebar-surface")
          ?? surface?.getAttribute("data-surface-id")
          ?? null,
      };
    };
    const active = document.activeElement;
    return {
      phase: failurePhase,
      documentHasFocus: document.hasFocus(),
      preKey: window.__tinePageArrowDownPreKey ?? null,
      keyWitness: window.__tinePageArrowDownKeyWitness ?? null,
      inputTrace: window.__tinePageHeaderInputTrace ?? null,
      compositionTrace: window.__tinePageHeaderCompositionTrace ?? null,
      active: {
        tag: active?.tagName ?? null,
        editor: describeEditor(active),
      },
      editors: [...document.querySelectorAll(".page-blocks textarea")].map(describeEditor),
    };
  }, phase);
}

async function preparePageHeaderArrowDown(expectedValue) {
  return browser.execute((expected) => {
    const header = document.querySelector(".page-blocks textarea.block-editor");
    if (header instanceof HTMLTextAreaElement) {
      header.focus();
      header.setSelectionRange(header.value.length, header.value.length);
    }
    const active = document.activeElement;
    const preKey = {
      documentHasFocus: document.hasFocus(),
      isPageHeader: active === header,
      value: active instanceof HTMLTextAreaElement ? active.value : null,
      selection: active instanceof HTMLTextAreaElement ? [active.selectionStart, active.selectionEnd] : null,
      delegatedKeydown: header instanceof HTMLTextAreaElement ? typeof header.$$keydown : null,
      expectedValue: expected,
      expectedSelection: [expected.length, expected.length],
    };
    window.__tinePageArrowDownPreKey = preKey;
    window.__tinePageArrowDownKeyWitness = null;
    document.addEventListener("keydown", (event) => {
      const target = event.target;
      const textarea = target instanceof HTMLTextAreaElement ? target : null;
      const block = textarea?.closest("[data-block-id]");
      const surface = textarea?.closest("[data-pane-id], [data-sidebar-surface], [data-surface-id]");
      const witness = {
        key: event.key,
        code: event.code,
        keyCode: event.keyCode,
        which: event.which,
        isTrusted: event.isTrusted,
        flags: {
          shift: event.shiftKey,
          ctrl: event.ctrlKey,
          alt: event.altKey,
          meta: event.metaKey,
          repeat: event.repeat,
          composing: event.isComposing,
          bubbles: event.bubbles,
          cancelable: event.cancelable,
        },
        target: textarea ? {
          value: textarea.value,
          blockId: block?.getAttribute("data-block-id") ?? null,
          surfaceId: surface?.getAttribute("data-pane-id")
            ?? surface?.getAttribute("data-sidebar-surface")
            ?? surface?.getAttribute("data-surface-id")
            ?? null,
        } : null,
      };
      queueMicrotask(() => {
        window.__tinePageArrowDownKeyWitness = { ...witness, defaultPrevented: event.defaultPrevented };
      });
    }, { capture: true, once: true });
    window.addEventListener("keydown", () => {
      queueMicrotask(() => {
        if (window.__tinePageArrowDownKeyWitness) window.__tinePageArrowDownKeyWitness.reachedWindowBubble = true;
      });
    }, { once: true });
    return preKey;
  }, expectedValue);
}

async function pageHeaderArrowDownReachedBody() {
  return browser.execute(() => {
    const active = document.activeElement;
    return active instanceof HTMLTextAreaElement
      && active.closest(".page-blocks") !== null
      && active.classList.contains("block-editor")
      && active.value === "Example content block";
  });
}

function deliveredExpectedArrowDown(capsule, expectedValue) {
  const witness = capsule?.keyWitness;
  return witness?.key === "ArrowDown"
    && witness.target?.value === expectedValue
    && Object.values(witness.flags ?? {}).every((flag) => flag === false || flag === true)
    && !witness.flags?.shift
    && !witness.flags?.ctrl
    && !witness.flags?.alt
    && !witness.flags?.meta
    && !witness.flags?.repeat
    && !witness.flags?.composing;
}

async function drivePageHeaderArrowDown(expectedValue) {
  const attempts = [];
  for (let attempt = 1; attempt <= 2; attempt += 1) {
    // JS focus alone can produce an activeElement inside a background WebView.
    // A real WebDriver pointer click first transfers native focus to the app;
    // let the freshly materialized editor finish its reactive ownership turn
    // before preparePageHeaderArrowDown restores the semantic end-of-header
    // caret. A human click-to-key gesture naturally includes this interval.
    const header = await browser.$(".page-blocks textarea.block-editor");
    await header.click();
    await sleep(100);
    const preKey = await preparePageHeaderArrowDown(expectedValue);
    if (
      !preKey.documentHasFocus
      || !preKey.isPageHeader
      || preKey.value !== expectedValue
      || preKey.selection?.[0] !== expectedValue.length
      || preKey.selection?.[1] !== expectedValue.length
    ) {
      attempts.push(await pageArrowDownCapsule(`readiness-${attempt}`));
      await sleep(75);
      continue;
    }

    await nativeArrowDown();
    const deadline = Date.now() + 1_250;
    let capsule;
    while (Date.now() < deadline) {
      if (await pageHeaderArrowDownReachedBody()) return;
      capsule = await pageArrowDownCapsule(`post-key-${attempt}`);
      if (capsule.keyWitness) break;
      await sleep(25);
    }
    // Once the expected unmodified ArrowDown has reached the expected header,
    // give reactive focus handoff the remainder of the bounded observation turn.
    if (deliveredExpectedArrowDown(capsule, expectedValue)) {
      while (Date.now() < deadline) {
        if (await pageHeaderArrowDownReachedBody()) return;
        await sleep(25);
      }
      attempts.push(await pageArrowDownCapsule(`delivered-${attempt}`));
      await sleep(75);
      continue;
    }
    attempts.push(capsule ?? await pageArrowDownCapsule(`undelivered-${attempt}`));
    await sleep(75);
  }
  // A single delivered event can still race WebKitGTK's editor ownership turn;
  // require the semantic handoff on either of two independently prepared native
  // actions. If either action was delivered but both attempts failed, preserve
  // the semantic failure instead of laundering it into infrastructure noise.
  if (attempts.some((capsule) => deliveredExpectedArrowDown(capsule, expectedValue))) {
    throw new Error(`PAGE_HEADER_ARROWDOWN_DELIVERED_BUT_IGNORED ${JSON.stringify(attempts)}`);
  }
  // The release runner may retry this isolated scenario once, but only when
  // neither independently prepared native action reached the expected editor.
  throw new Error(`E2E_NATIVE_INPUT_UNDELIVERED page-properties ArrowDown ${JSON.stringify(attempts)}`);
}

async function replaceHeaderLikeUser(editor, replacement, selection = null) {
  // WebdriverIO's setValue() first clears the textarea as a separate WebDriver
  // command. Clearing an existing header and blurring is a real delete action,
  // so Tine correctly removes that transient editor before setValue() can issue
  // its second (send-keys) command. A user replaces text in one edit session:
  // select it, then type. Exercise that native sequence and prove there was no
  // artificial empty input between the two actions.
  await browser.execute(() => {
    window.__tinePageHeaderInputTrace = [];
    window.__tinePageHeaderCompositionTrace = [];
    const textarea = document.querySelector(".page-blocks textarea.block-editor");
    for (const type of ["compositionstart", "compositionupdate", "compositionend"]) {
      textarea?.addEventListener(type, (event) => {
        window.__tinePageHeaderCompositionTrace.push({ type, data: event.data });
      }, { capture: true });
    }
    textarea?.addEventListener("input", (event) => {
      window.__tinePageHeaderInputTrace.push({
        inputType: event.inputType,
        data: event.data,
        isComposing: event.isComposing,
        value: event.currentTarget.value,
      });
    }, { capture: true });
  });
  await editor.click();
  if (selection) {
    await browser.execute(({ start, end }) => {
      const textarea = document.querySelector(".page-blocks textarea.block-editor");
      if (!(textarea instanceof HTMLTextAreaElement)) throw new Error("missing page-header editor");
      textarea.focus();
      textarea.setSelectionRange(start, end);
    }, selection);
  } else {
    await nativeSelectAll();
  }
  await editor.addValue(replacement);
  const trace = await browser.execute(() => window.__tinePageHeaderInputTrace ?? []);
  if (
    trace.length === 0
    || trace[0].data !== replacement[0]
    || trace[0].value === ""
    || trace.some((entry) => entry.value === "")
  ) {
    throw new Error(`native page-header replacement emitted an empty intermediate input: ${JSON.stringify(trace)}`);
  }
  return trace;
}

async function deleteHeaderLikeUser(editor) {
  await editor.click();
  await nativeSelectAll();
  await browser.keys(["Backspace"]);
  if (await editor.getValue() !== "") {
    throw new Error(`native page-header deletion did not empty the editor: ${JSON.stringify(await editor.getValue())}`);
  }
  await browser.execute(() => {
    const textarea = document.querySelector(".page-blocks textarea.block-editor");
    if (!(textarea instanceof HTMLTextAreaElement)) throw new Error("missing page-header editor");
    textarea.blur();
  });
  await editor.waitForExist({ reverse: true, timeout: 5_000 });
}

async function activePagePropertyControl() {
  return browser.execute(() => {
    const panel = document.querySelector(".page-props-panel");
    const active = document.activeElement;
    if (!panel || !active || !panel.contains(active)) return null;
    if (active.classList.contains("pp-input")) {
      return active.closest(".pp-field")?.querySelector(".pp-label")?.textContent?.trim() ?? null;
    }
    if (active.matches(".pp-bool input")) return active.closest(".pp-field")?.querySelector(".pp-label")?.textContent?.trim() ?? null;
    return active.classList.contains("pp-done") ? "Done" : null;
  });
}

async function exerciseNativeFormTabTraversal(aliasValue) {
  // Pointer-open the actual routed named-page panel; the key actions below are
  // W3C native actions, not synthetic DOM events, so WebKit performs its focus
  // default action if the capture handler leaves it alone.
  await openPageProperties();
  const aliases = (await browser.$$(".page-props-panel .pp-input"))[0];
  await aliases.click();
  await aliases.setValue(aliasValue);

  await nativeTab();
  if (await activePagePropertyControl() !== "Tags") {
    throw new Error(`native Tab did not leave Aliases for Tags; active=${JSON.stringify(await activePagePropertyControl())}`);
  }
  await waitForFile(
    `${GRAPH}/pages/Property detailed.md`,
    (text) => text.includes(`alias:: ${aliasValue}`),
    "Aliases blur commit after native Tab",
  );

  await nativeTab({ shift: true });
  if (await activePagePropertyControl() !== "Aliases") {
    throw new Error(`native Shift+Tab did not return to Aliases; active=${JSON.stringify(await activePagePropertyControl())}`);
  }

  for (const expected of ["Tags", "Display title", "Icon", "Public", "Done"]) {
    await nativeTab();
    if (await activePagePropertyControl() !== expected) {
      throw new Error(`native Tab focus order expected ${expected}; active=${JSON.stringify(await activePagePropertyControl())}`);
    }
  }
  await browser.$(".pp-done").click();
  await browser.$(".page-props-panel").waitForExist({ reverse: true, timeout: 5_000 });
}

try {
  browser = await remote({
    hostname: "127.0.0.1",
    port: DRIVER,
    path: "/",
    logLevel: "error",
    connectionRetryCount: 1,
    connectionRetryTimeout: 60_000,
    capabilities: tauriCapabilities(APP, "default", process.platform, webviewTarget.debuggerAddress),
  });
  // This fixture starts on today's journal so it has a durable navigation
  // source for the seeded pages. Journals render blocks (and a journal title),
  // not a named-page `.page-title`; waiting for the latter prevented openPage()
  // from ever exercising the routed page-properties journey on WebView2.
  await browser.$(".ls-block, .journal-title, .page-title").waitForExist({ timeout: 20_000 });
  await openPage("Property detailed");
  await exerciseNativeFormTabTraversal("Test Record, Alternate");
  const customRow = await browser.execute(() => {
    const rows = [...document.querySelectorAll(".page-properties .prop-row")];
    const row = rows.find((element) => element.querySelector(".prop-key")?.textContent?.trim() === "ai-prompt");
    if (!row) return false;
    row.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, button: 0 }));
    return true;
  });
  if (!customRow) throw new Error("missing rendered custom page property authoring target");
  let headerEditor = await browser.$(".page-blocks textarea.block-editor");
  await headerEditor.waitForExist({ timeout: 5_000 });
  const originalHeader = await headerEditor.getValue();
  if (!originalHeader.includes("ai-prompt:: [[Prompt-Test]]") || !originalHeader.includes("\n\npage-level::")) {
    throw new Error(`page-header ordinary editor lost raw properties/separators: ${JSON.stringify(originalHeader)}`);
  }
  // Keep this native replacement independent of autocomplete. Editing inside a
  // page reference opens a second interaction lifecycle whose close timing can
  // obscure the separate page-header ArrowDown contract.
  const oldTimestamp = "20250707092601";
  const newTimestamp = "20260707092601";
  const editedHeader = originalHeader.replace(oldTimestamp, newTimestamp);
  const oldTimestampStart = originalHeader.indexOf(oldTimestamp);
  const replacementTrace = await replaceHeaderLikeUser(headerEditor, newTimestamp, {
    start: oldTimestampStart,
    end: oldTimestampStart + oldTimestamp.length,
  });
  if ((await headerEditor.getValue()) !== editedHeader) {
    throw new Error(`native page-header replacement did not preserve the intended value: ${JSON.stringify({ replacementTrace, actual: await headerEditor.getValue() })}`);
  }
  await drivePageHeaderArrowDown(editedHeader);
  await browser.execute(() => {
    const editor = document.querySelector(".page-blocks textarea.block-editor");
    editor?.focus();
    if (editor instanceof HTMLTextAreaElement) editor.setSelectionRange(2, 2);
  });
  await browser.keys(["ArrowUp"]);
  await browser.waitUntil(async () => (await browser.$(".page-blocks textarea.block-editor").getValue()).includes(newTimestamp), {
    timeout: 5_000,
    timeoutMsg: "Arrow Up did not cross from the first body block back into the page header",
  });
  await browser.execute(() => {
    const textarea = document.querySelector(".page-blocks textarea.block-editor");
    if (!(textarea instanceof HTMLTextAreaElement)) throw new Error("missing page-header editor");
    textarea.blur();
  });
  const detailedAfter = await waitForFile(
    `${GRAPH}/pages/Property detailed.md`,
    (text) => text.includes("alias:: Test Record, Alternate") && text.includes(`timestamp:: ${newTimestamp}`),
    "detailed page property edit",
  );
  const detailedExpected = detailed
    .replace("alias:: Test Record", "alias:: Test Record, Alternate")
    .replace(oldTimestamp, newTimestamp);
  if (detailedAfter !== detailedExpected) {
    throw new Error(`detailed page changed outside the edited line\nEXPECTED:\n${detailedExpected}\nACTUAL:\n${detailedAfter}`);
  }

  await openPage("Property simple");
  await setPagePropertyField("Icon", "★");
  const simpleAfter = await waitForFile(
    `${GRAPH}/pages/Property simple.md`,
    (text) => text.includes("icon:: ★"),
    "simple page property edit",
  );
  const simpleExpected = "icon:: ★\r\nA:: XX\r\nB:: XX\r\nC:: XX\r\n";
  if (simpleAfter !== simpleExpected) {
    throw new Error(`simple CRLF page changed outside OG's prepended property line\nEXPECTED:\n${JSON.stringify(simpleExpected)}\nACTUAL:\n${JSON.stringify(simpleAfter)}`);
  }
  const lines = simpleAfter.trimEnd().split(/\r?\n/);
  if (lines.some((line) => /^\s*-\s/.test(line) || /^\s{2,}\S/.test(line))) {
    throw new Error(`header properties became outline content: ${JSON.stringify(lines)}`);
  }
  if (!lines.includes("A:: XX") || !lines.includes("B:: XX") || !lines.includes("C:: XX") || !lines.includes("icon:: ★")) {
    throw new Error(`simple page properties were lost or merged: ${JSON.stringify(lines)}`);
  }
  await openPage("Property detailed");
  const reopenedCustom = await browser.execute(() => [...document.querySelectorAll(".page-properties .prop-row")]
    .find((row) => row.querySelector(".prop-key")?.textContent?.trim() === "ai-prompt")
    ?.querySelector(".prop-value")?.textContent?.trim() ?? null);
  if (!reopenedCustom?.includes("Prompt-Test")) {
    throw new Error(`reopened page did not parse the edited custom header: ${JSON.stringify(reopenedCustom)}`);
  }
  await openPage("Property deletion");
  const deleteTarget = await browser.execute(() => {
    const rows = [...document.querySelectorAll(".page-properties .prop-row")];
    const row = rows.find((element) => element.querySelector(".prop-key")?.textContent?.trim() === "custom/key");
    if (!row) return false;
    row.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, button: 0 }));
    return true;
  });
  if (!deleteTarget) throw new Error("missing rendered page-header deletion target");
  headerEditor = await browser.$(".page-blocks textarea.block-editor");
  await headerEditor.waitForExist({ timeout: 5_000 });
  const selectAllTrace = await replaceHeaderLikeUser(headerEditor, "alias:: Delete Me Later");
  if ((await headerEditor.getValue()) !== "alias:: Delete Me Later") {
    throw new Error(`Ctrl+A page-header replacement did not preserve the intended value: ${JSON.stringify({ traceLength: selectAllTrace.length, actual: await headerEditor.getValue() })}`);
  }
  await browser.execute(() => {
    const textarea = document.querySelector(".page-blocks textarea.block-editor");
    if (!(textarea instanceof HTMLTextAreaElement)) throw new Error("missing page-header editor");
    textarea.blur();
  });
  const replacementAfter = await waitForFile(
    `${GRAPH}/pages/Property deletion.md`,
    (text) => text.startsWith("alias:: Delete Me Later\n\n"),
    "Ctrl+A page-header replacement",
  );
  if (replacementAfter !== "alias:: Delete Me Later\n\n- Body survives\n") {
    throw new Error(`Ctrl+A page-header replacement changed body bytes: ${JSON.stringify(replacementAfter)}`);
  }
  await openPage("Property deletion");
  const replacedTarget = await browser.execute(() => {
    const row = [...document.querySelectorAll(".page-aliases, .page-properties .prop-row")]
      .find((element) => element.textContent?.includes("Delete Me Later"));
    if (!row) return false;
    row.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, button: 0 }));
    return true;
  });
  if (!replacedTarget) throw new Error("missing replaced page-header deletion target");
  headerEditor = await browser.$(".page-blocks textarea.block-editor");
  await headerEditor.waitForExist({ timeout: 5_000 });
  await deleteHeaderLikeUser(headerEditor);
  const deletionAfter = await waitForFile(
    `${GRAPH}/pages/Property deletion.md`,
    (text) => !text.includes("alias::") && !text.includes("custom/key::"),
    "deliberate page-header deletion",
  );
  if (deletionAfter !== "- Body survives\n") {
    throw new Error(`deleting the page header changed body bytes: ${JSON.stringify(deletionAfter)}`);
  }
  await openPage("Property deletion");
  if ((await browser.$$(".page-properties .prop-row")).length !== 0) {
    throw new Error("deleted page-header properties reappeared after real-app reopen");
  }
  console.log(`PASS: page-header click/edit/navigation, native replacements (${replacementTrace.length}+${selectAllTrace.length} input events), deletion, disk bytes, and real-app reopen are canonical`);
} finally {
  try { await browser?.deleteSession(); } catch {}
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(driver.pid), "/T", "/F"], { stdio: "ignore" });
    if (process.env.CI === "true") {
      spawnSync("taskkill", ["/IM", path.basename(APP), "/T", "/F"], { stdio: "ignore" });
    }
  } else try { process.kill(-driver.pid, "SIGKILL"); } catch {}
  stopWebdriverApplication(webviewTarget);
  fs.closeSync(log);
}
