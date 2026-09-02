import { waitForHttpServer } from "./e2e-capabilities.mjs";
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const port = 5201;
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const server = spawn(
  path.join(root, "node_modules", ".bin", "vite"),
  ["preview", "--host", "127.0.0.1", "--port", String(port), "--strictPort"],
  { cwd: root, stdio: "ignore" },
);


const regular = (id, content, nested = "") => `
  <div class="ls-block" id="${id}">
    <div class="block-main">
      <div class="block-controls">
        <span class="collapse-toggle"></span>
        <span class="bullet-container"><span class="bullet"></span></span>
      </div>
      <div class="block-content-wrapper"><div class="block-content has-bg" style="background:#587763">${content}</div></div>
    </div>
    ${nested}
  </div>`;

const ordered = `
  <div class="ls-block" id="ordered">
    <div class="block-main">
      <div class="block-controls">
        <span class="collapse-toggle"></span>
        <span class="bullet-container ordered"><span class="bullet-order">1.</span></span>
      </div>
      <div class="block-content-wrapper"><div class="block-content has-bg" style="background:#675173">Numbered</div></div>
    </div>
  </div>`;

const editing = `
  <div class="ls-block" id="editing">
    <div class="block-main editing">
      <div class="block-controls">
        <span class="collapse-toggle"></span>
        <span class="bullet-container"><span class="bullet"></span></span>
      </div>
      <div class="block-content-wrapper"><textarea class="block-editor">Editing</textarea></div>
    </div>
  </div>`;

try {
  const url = `http://127.0.0.1:${port}/`;
  await waitForHttpServer(url, 60, 250, { failureMessage: "preview server did not start" });
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  await page.goto(url);
  await page.evaluate(({ regularHtml, orderedHtml, editingHtml }) => {
    document.body.innerHTML = `<main class="page-section" style="width:100%;max-width:720px;padding:0 12px;box-sizing:border-box">${regularHtml}${orderedHtml}${editingHtml}</main>`;
  }, {
    regularHtml: regular("regular", "Regular", `<div class="block-children">${regular("nested", "Nested")}</div>`),
    orderedHtml: ordered,
    editingHtml: editing,
  });

  const geometry = async (id) => page.locator(`#${id}`).evaluate((node) => {
    const main = node.querySelector(":scope > .block-main");
    const control = main?.querySelector(".bullet-container");
    const dot = control?.querySelector(".bullet");
    const content = main?.querySelector(".block-content.has-bg");
    const editor = main?.querySelector(".block-editor");
    const wrapper = main?.querySelector(".block-content-wrapper");
    if (!main || !control || !wrapper || (!content && !editor)) throw new Error("layout fixture is incomplete");
    const controlStyle = getComputedStyle(control);
    const controlRect = control.getBoundingClientRect();
    const wrapperLeft = wrapper.getBoundingClientRect().left;
    const subject = content ?? editor;
    const subjectRect = subject.getBoundingClientRect();
    const subjectStyle = getComputedStyle(subject);
    const dotRect = dot?.getBoundingClientRect();
    return {
      controlTrack: controlRect.width + parseFloat(controlStyle.marginRight),
      dotGap: dotRect && content ? subjectRect.left - dotRect.right : null,
      textStart: subjectRect.left + parseFloat(subjectStyle.paddingLeft),
      wrapperLeft,
      backgroundRadius: content ? subjectStyle.borderRadius : null,
    };
  });

  const measure = async () => ({
    regular: await geometry("regular"),
    ordered: await geometry("ordered"),
    nested: await geometry("nested"),
    editing: await geometry("editing"),
    viewport: await page.evaluate(() => {
      const clientWidth = document.documentElement.clientWidth;
      const widest = [...document.querySelectorAll("body *")]
        .map((element) => ({ id: element.id, className: String(element.className), right: element.getBoundingClientRect().right }))
        .sort((a, b) => b.right - a.right)[0];
      return { clientWidth, scrollWidth: document.documentElement.scrollWidth, widest };
    }),
  });
  const assertGeometry = (label, values) => {
    for (const [name, value] of Object.entries(values)) {
      if (name === "viewport") continue;
      if (Math.abs(value.controlTrack - 22) > 0.1 || Math.abs(value.textStart - value.wrapperLeft) > 0.1) {
        throw new Error(`${label} ${name} alignment failed: ${JSON.stringify(value)}`);
      }
    }
    for (const name of ["regular", "nested"]) {
      if ((values[name].dotGap ?? 0) < 6.5 || values[name].backgroundRadius !== "5px") {
        throw new Error(`${label} ${name} clearance failed: ${JSON.stringify(values[name])}`);
      }
    }
    if (values.viewport.scrollWidth > values.viewport.clientWidth) {
      throw new Error(`${label} fixture overflowed horizontally: ${JSON.stringify(values.viewport)}`);
    }
  };

  const desktop = await measure();
  assertGeometry("desktop", desktop);
  await page.setViewportSize({ width: 390, height: 844 });
  const mobile = await measure();
  assertGeometry("mobile", mobile);

  console.log(`block spacing check passed: ${JSON.stringify({ desktop, mobile })}`);
  await browser.close();
} finally {
  server.kill("SIGTERM");
}
