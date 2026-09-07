import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Screenshot how an ADVANCED (datalog) query renders: no resting sentence and no
// sheet — datalog is not builder-representable and this campaign deliberately
// does not convert it (§4.3.1, Q13) — but the "Partial datalog — ran: …" note
// shows and the results still render. Headless Chromium over the mock backend.
//
// This used to click a `⚙ advanced` button that turned a filter query INTO a
// datalog one. That affordance was removed before this packet (the frontend has
// no datalog printer left to write the skeleton with), and the mock fixture has
// no datalog block to find instead: only the ENGINE decides that a `{{query}}`
// holds datalog, and the browser mock has no engine. So the advanced reading is
// installed as canned DATA through the mock-only fixture seam
// (`src/mockQueryFixture.guard.test.ts` keeps it out of production), exactly as
// `shot-query-sheet.mjs` does.
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5263;
const OUT = "screenshots";
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });


try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 40, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  const page = await browser.newPage({ viewport: { width: 1200, height: 1300 }, deviceScaleFactor: 2 });
  page.on("pageerror", (e) => console.log("pageerror:", String(e).split("\n")[0]));

  await page.addInitScript(() => {
    globalThis.__tineMockQueryFixture = {
      parse: {
        query: {
          anchor: "block",
          filter: { kind: "raw", text: "(and (task TODO) (sample 5))", kind_of: "unknown_head" },
          diagnostics: [],
          source: {
            kind: "advanced",
            original: "#+BEGIN_QUERY\n{:title \"Recent tasks\"\n :query [:find (pull ?b [*])\n         :where [?b :block/marker \"TODO\"]]}\n#+END_QUERY",
            og_options: "",
          },
        },
        view: {},
      },
      run: {
        anchor: "block",
        groups: [],
        diagnostics: [],
        report: { ran: ["task"], ignored: ["(sample 5)"], supported: true },
        total: 0,
        exceeded: false,
      },
    };
  });

  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".page-title", { timeout: 8000 });
  await sleep(500);

  const qblk = page.locator(".query-block").filter({ has: page.locator(".query-adv-note") }).first();
  await qblk.waitFor({ timeout: 8000 });
  const sentences = await page.locator(".qs-sentence").count();
  const advSentences = await qblk.locator(".qs-sentence").count();
  console.log("query sentences on the page:", sentences, "| inside the advanced block:", advSentences,
    "(must be 0 — an advanced query has no sentence and no sheet)");
  const note = await qblk.locator(".query-adv-note").first().innerText().catch(() => "(no note)");
  console.log("adv note:", note.replace(/\n/g, " "));
  // The advanced query still runs and still counts, in the HEADER.
  const count = await qblk.locator(".query-header .query-count").first().innerText().catch(() => "?");
  console.log("result count on the advanced query:", count);

  await qblk.scrollIntoViewIfNeeded();
  await sleep(200);
  const box = await qblk.boundingBox();
  if (box) {
    await page.screenshot({
      path: `${OUT}/advanced-switch.png`,
      clip: { x: Math.max(0, box.x - 8), y: Math.max(0, box.y - 8), width: Math.min(1200, box.width + 16), height: Math.min(1290 - Math.max(0, box.y - 8), box.height + 16) },
    });
    console.log(`wrote ${OUT}/advanced-switch.png`);
  }

  await browser.close();
  console.log("DONE");
} catch (e) {
  console.log("ERROR:", String(e).split("\n").slice(0, 3).join(" | "));
  process.exitCode = 2;
} finally {
  server.kill("SIGTERM");
}
