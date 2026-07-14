import { chromium } from "playwright";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const css = ["theme.css", "app.css"]
  .map((file) => fs.readFileSync(path.join(root, "src/styles", file), "utf8"))
  .join("\n");
const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
  });
  const nestedOutline = Array.from({ length: 8 }, (_, index) => index)
    .reduceRight((children, depth) => `
      <div class="ls-block" data-depth="${depth}">
        <div class="block-main">
          <div class="block-controls">
            <span class="collapse-toggle${children ? " has-children" : ""}"><svg class="triangle"></svg></span>
            <span class="bullet-container"><span class="bullet"></span></span>
          </div>
          <div class="block-content-wrapper"><div class="block-content">Depth ${depth} keeps enough room for useful text.</div></div>
        </div>
        ${children ? `<div class="block-children-container"><button class="block-children-left-border"></button><div class="block-children">${children}</div></div>` : ""}
      </div>`, "");
  await page.setContent(`<!doctype html><meta name="viewport" content="width=device-width, initial-scale=1"><style>${css}</style><main class="main-content"><div class="main-content-inner"><h1>Mobile outline</h1>${nestedOutline}</div></main>`);
  const geometry = await page.locator(".main-content-inner").evaluate((element) => {
    const style = getComputedStyle(element);
    const rect = element.getBoundingClientRect();
    return {
      width: rect.width,
      paddingLeft: Number.parseFloat(style.paddingLeft),
      paddingRight: Number.parseFloat(style.paddingRight),
      usable: rect.width - Number.parseFloat(style.paddingLeft) - Number.parseFloat(style.paddingRight),
    };
  });
  if (geometry.paddingLeft > 16 || geometry.paddingRight > 16 || geometry.usable < 350) {
    throw new Error(`mobile content remains too narrow: ${JSON.stringify(geometry)}`);
  }
  const nesting = await page.evaluate(() => {
    const root = document.querySelector('[data-depth="0"]');
    const deepest = document.querySelector('[data-depth="7"] .block-content');
    const rootContent = document.querySelector('[data-depth="0"] > .block-main .block-content');
    const fold = document.querySelector('[data-depth="0"] > .block-main .collapse-toggle');
    const bullet = document.querySelector('[data-depth="0"] > .block-main .bullet-container');
    const guide = document.querySelector('[data-depth="0"] > .block-children-container > .block-children');
    if (!(root && deepest && rootContent && fold && bullet && guide)) throw new Error("mobile nesting fixture is incomplete");
    const rootRect = root.getBoundingClientRect();
    const deepestRect = deepest.getBoundingClientRect();
    const rootContentRect = rootContent.getBoundingClientRect();
    const foldRect = fold.getBoundingClientRect();
    const bulletRect = bullet.getBoundingClientRect();
    const guideRect = guide.getBoundingClientRect();
    return {
      deepestInset: deepestRect.left - rootRect.left,
      deepestWidth: deepestRect.width,
      depthInsets: Array.from(document.querySelectorAll("[data-depth]"), (element) => ({
        depth: element.getAttribute("data-depth"),
        left: element.getBoundingClientRect().left - rootRect.left,
      })),
      foldWidth: foldRect.width,
      foldIsTrailing: foldRect.left >= rootContentRect.right - foldRect.width,
      guideOffset: Math.abs(guideRect.left - (bulletRect.left + bulletRect.width / 2)),
      guideLeft: guideRect.left,
      bulletCenter: bulletRect.left + bulletRect.width / 2,
    };
  });
  if (nesting.deepestInset > 165 || nesting.deepestWidth < 180) {
    throw new Error(`deep mobile nesting still consumes too much text width: ${JSON.stringify(nesting)}`);
  }
  if (nesting.foldWidth < 40 || !nesting.foldIsTrailing) {
    throw new Error(`mobile fold control is not a trailing touch affordance: ${JSON.stringify(nesting)}`);
  }
  if (nesting.guideOffset > 1.5) {
    throw new Error(`mobile nesting guide is not aligned with its parent bullet: ${JSON.stringify(nesting)}`);
  }
  const desktopPage = await browser.newPage({ viewport: { width: 390, height: 844 } });
  await desktopPage.setContent(`<!doctype html><meta name="viewport" content="width=device-width, initial-scale=1"><style>${css}</style><main class="main-content"><div class="main-content-inner">${nestedOutline}</div></main>`);
  const desktop = await desktopPage.evaluate(() => {
    const root = document.querySelector('[data-depth="0"]');
    const child = document.querySelector('[data-depth="1"]');
    const fold = document.querySelector('[data-depth="0"] > .block-main .collapse-toggle');
    if (!(root && child && fold)) throw new Error("desktop nesting fixture is incomplete");
    return {
      indent: child.getBoundingClientRect().left - root.getBoundingClientRect().left,
      foldWidth: fold.getBoundingClientRect().width,
    };
  });
  await desktopPage.close();
  if (desktop.indent !== 29 || desktop.foldWidth !== 18) {
    throw new Error(`precise-pointer outline geometry changed with the mobile fix: ${JSON.stringify(desktop)}`);
  }
  fs.mkdirSync(path.join(root, "test-results"), { recursive: true });
  await page.screenshot({ path: path.join(root, "test-results/mobile-content-width.png"), fullPage: true });
  console.log(`PASS: mobile content uses ${geometry.usable}px; depth-8 text keeps ${nesting.deepestWidth}px`);
} finally {
  await browser.close();
}
