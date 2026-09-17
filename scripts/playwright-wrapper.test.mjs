// What the Playwright launch wrapper may and may not refuse.
//
// Its first version threw before launching whenever PLAYWRIGHT_BROWSERS_PATH
// was unset. That variable is set only by `scripts/env.sh`, and nothing in the
// E2E entry path sources env.sh: the hosted Linux job installs browsers with
// `npx playwright install` into Playwright's own default location and never
// sets it. So the check did not detect a broken environment — it declared the
// supported CI configuration broken, and locally it failed selection-wrap,
// publish-security and published-app before a browser was ever asked for.
//
// A precondition is not evidence: assert the property, and let the launch that
// genuinely cannot find a browser be the thing that speaks. Restoring the
// precondition fails three of the four tests below.
//
// The companion `src/playwrightImports.guard.test.ts` keeps every script
// importing this wrapper rather than the package; this file pins what the
// wrapper then does.
import test from "node:test";
import assert from "node:assert/strict";
import { REMEDY, guarded } from "./lib/playwright.mjs";

test("launches with no PLAYWRIGHT_BROWSERS_PATH, because the hosted runner has none", async () => {
  const browser = { marker: "launched" };
  const previous = process.env.PLAYWRIGHT_BROWSERS_PATH;
  delete process.env.PLAYWRIGHT_BROWSERS_PATH;
  try {
    assert.equal(await guarded({ launch: async () => browser }, "chromium").launch(), browser);
  } finally {
    if (previous !== undefined) process.env.PLAYWRIGHT_BROWSERS_PATH = previous;
  }
});

test("forwards the caller's launch options", async () => {
  const seen = [];
  const stub = { launch: async (...args) => { seen.push(...args); return {}; } };
  await guarded(stub, "chromium").launch({ headless: true });
  assert.deepEqual(seen, [{ headless: true }]);
});

test("names the remedy when the browser is genuinely missing", async () => {
  const stub = {
    launch: async () => {
      throw new Error("Executable doesn't exist at /x/ms-playwright/chromium-1234/chrome-linux/chrome");
    },
  };
  await assert.rejects(
    guarded(stub, "chromium").launch(),
    (error) => error.message.includes(REMEDY) && error.message.includes("Executable doesn't exist"),
  );
});

test("passes an unrelated failure through unchanged", async () => {
  const original = new Error("Target page, context or browser has been closed");
  const stub = { launch: async () => { throw original; } };
  await assert.rejects(guarded(stub, "chromium").launch(), (error) => error === original);
});
