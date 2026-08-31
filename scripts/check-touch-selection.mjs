// GH #452: long-pressing a page link on iOS raised the platform's Copy/Look Up
// bar over Tine's own context menu, with the menu's first action highlighted as
// selected text.
//
// The hold that opens Tine's menu is also the OS's text-selection gesture, and
// that gesture belongs to a native recognizer: `preventDefault()` on our
// synthetic `contextmenu` cannot cancel it, and iOS does not fire `contextmenu`
// for touch at all. The only thing that stops it is refusing to be selectable.
// Two separate rules do that, and this checks both:
//
//   1. the menu is chrome, never text — on every platform;
//   2. a long-pressable link is unselectable on touch platforms only, so a
//      desktop mouse can still drag across a page link's text.
//
// Computed style in a real engine, because that is where the cascade actually
// resolves; jsdom applies no CSS.
import { chromium } from "playwright";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const css = ["theme.css", "app.css"]
  .map((file) => fs.readFileSync(path.join(root, "src/styles", file), "utf8"))
  .join("\n");

const markup = `
  <div class="block-body"><a class="page-ref">Some page</a> and a <a class="tag">#tag</a></div>
  <div class="ctx-overlay">
    <div class="ctx-menu">
      <div class="ctx-item">Open in the sidebar</div>
      <div class="ctx-item">Copy link</div>
    </div>
  </div>`;

const selectable = (value) => value !== "none";

const browser = await chromium.launch({
  headless: true,
  args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"],
});
try {
  const page = await browser.newPage({ viewport: { width: 480, height: 640 } });

  const read = (platform) =>
    page.evaluate((value) => {
      if (value) document.documentElement.setAttribute("data-platform", value);
      else document.documentElement.removeAttribute("data-platform");
      const of = (selector) => {
        const style = getComputedStyle(document.querySelector(selector));
        return style.webkitUserSelect || style.userSelect;
      };
      return {
        pageRef: of(".page-ref"),
        tag: of(".tag"),
        menu: of(".ctx-menu"),
        item: of(".ctx-item"),
        overlay: of(".ctx-overlay"),
      };
    }, platform);

  await page.setContent(`<!doctype html><style>${css}</style>${markup}`);

  for (const platform of ["ios", "android"]) {
    const s = await read(platform);
    if (selectable(s.pageRef) || selectable(s.tag)) {
      throw new Error(
        `on ${platform} a long-press target is still selectable, so the OS selection gesture runs under Tine's menu: ${JSON.stringify(s)}`,
      );
    }
  }

  const desktop = await read("desktop");
  if (!selectable(desktop.pageRef) || !selectable(desktop.tag)) {
    throw new Error(
      `desktop lost ordinary text selection over links, which no report asked for: ${JSON.stringify(desktop)}`,
    );
  }

  for (const platform of ["ios", "android", "desktop", null]) {
    const s = await read(platform);
    if (selectable(s.menu) || selectable(s.item) || selectable(s.overlay)) {
      throw new Error(
        `the context menu is selectable text on ${platform ?? "an unstamped root"}: ${JSON.stringify(s)}`,
      );
    }
  }

  console.log(
    "PASS: context menus are never selectable; page links resist the touch selection gesture and keep desktop selection",
  );
} finally {
  await browser.close();
}
