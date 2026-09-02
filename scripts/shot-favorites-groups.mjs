import { waitForHttpServer } from "./e2e-capabilities.mjs";
// HARNESS DEBT — DOES NOT CURRENTLY RUN HERE. Kept because the check it wants
// to make is the right one, not because it works: every invocation on this box
// is killed (exit 144) before the script produces output, with no stray vite or
// chromium process and no PID pressure. Two debugging attempts were spent; the
// batch stop-loss (AGENTS.md 2b) says record the debt and move on rather than
// keep fighting the apparatus. Do not treat a green run of this file as
// evidence until someone has made it run at all.
//
// What it is FOR: jsdom applies no CSS layout, so the component tests can prove
// the `grouped` class is on the right rows but cannot prove the rows actually
// indent. That, and the group header's alignment, remain visually unverified.
//
// Screenshot the one-level Favorites groups in the left sidebar (GH #102).
// It asserts the wiring it photographs.
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5199;
const OUT = "screenshots";
mkdirSync(OUT, { recursive: true });

const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], {
  stdio: "inherit",
});


try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 40, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch({
    args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"],
  });
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 }, deviceScaleFactor: 2 });
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".left-sidebar-inner", { timeout: 8000 });

  // Favorite three pages, then arrange two of them into a group.
  await page.evaluate(async () => {
    const ui = await import("/src/ui.ts");
    const store = await import("/src/favoritesStore.ts");
    const layout = await import("/src/favoritesLayout.ts");
    ui.seedFavorites(["Alpha", "Beta", "Gamma"]);
    const block = (raw, children = []) => ({ id: raw, raw, collapsed: false, children });
    await store.persistFavoritesLayout(
      layout.layoutFromBlocks([
        block("[[Alpha]]"),
        block("Work", [block("[[Beta]]"), block("[[Gamma]]")]),
      ]),
    );
  });
  await sleep(400);

  const rows = await page.locator("#sidebar-favorites-list .nav-page").allInnerTexts();
  const groups = await page.locator(".nav-fav-group-name").inputValue();
  const addGroup = await page.locator(".nav-fav-add-group").count();
  await page.locator(".left-sidebar-inner").screenshot({ path: `${OUT}/favorites-groups.png` });

  // The grouped rows must actually be indented past the ungrouped one, which is
  // the whole thing jsdom cannot tell us.
  const [ungrouped, grouped] = await page.evaluate(() => {
    const all = [...document.querySelectorAll("#sidebar-favorites-list .nav-page")];
    const plain = all.find((el) => !el.classList.contains("grouped"));
    const inGroup = all.find((el) => el.classList.contains("grouped"));
    return [plain?.getBoundingClientRect().left ?? 0, inGroup?.getBoundingClientRect().left ?? 0];
  });

  // Collapsing the group hides its members but keeps them favorited.
  await page.locator(".nav-fav-group-toggle").click();
  await sleep(300);
  const collapsedRows = await page.locator("#sidebar-favorites-list .nav-page").count();
  await page.locator(".left-sidebar-inner").screenshot({ path: `${OUT}/favorites-groups-collapsed.png` });

  await browser.close();

  const problems = [];
  if (rows.length !== 3) problems.push(`expected 3 favorite rows, saw ${rows.length}`);
  if (groups !== "Work") problems.push(`expected a "Work" group header, saw ${JSON.stringify(groups)}`);
  if (addGroup !== 1) problems.push(`expected one add-group affordance, saw ${addGroup}`);
  if (!(grouped > ungrouped)) problems.push(`grouped rows are not indented (${grouped} vs ${ungrouped})`);
  if (collapsedRows !== 1) problems.push(`collapsing should leave 1 row, saw ${collapsedRows}`);
  if (problems.length) {
    console.error("FAIL:\n  " + problems.join("\n  "));
    process.exitCode = 1;
  } else {
    console.log("PASS: favorites groups render, indent and collapse");
    console.log(`  ${OUT}/favorites-groups.png`);
    console.log(`  ${OUT}/favorites-groups-collapsed.png`);
  }
} finally {
  server.kill();
}
