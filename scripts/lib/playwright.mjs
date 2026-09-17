// Playwright, with a launch failure that names its remedy.
//
// The toolchain lives OUTSIDE the repo in a sibling `.toolchain/`, and
// `scripts/env.sh` is what points `PLAYWRIGHT_BROWSERS_PATH` at it. A script
// run without sourcing env.sh therefore fails inside Playwright with
// "Executable doesn't exist at …/ms-playwright/chromium-…/chrome" and a
// suggestion to run `npx playwright install` — which is the wrong remedy: it
// would download a second copy of a browser this machine already has, into a
// directory nothing reads. The right remedy is one line, and the error should
// say it rather than leave each reader to rediscover it.
//
// Only `launch` is wrapped, because `launch` is the only member any script in
// this repository uses (`src/playwrightImports.guard.test.ts` keeps that true,
// and keeps new scripts importing this module rather than the package).
import { chromium as playwrightChromium, webkit as playwrightWebkit } from "playwright";

const REMEDY = "Run `source scripts/env.sh` first: the browsers live in the sibling .toolchain/ms-playwright, "
  + "outside the repo, and PLAYWRIGHT_BROWSERS_PATH is how Playwright finds them. Do NOT run "
  + "`npx playwright install` — it downloads a second copy into a directory nothing reads.";

function guarded(browserType, name) {
  return {
    async launch(...args) {
      if (!process.env.PLAYWRIGHT_BROWSERS_PATH) {
        throw new Error(`${name}.launch: PLAYWRIGHT_BROWSERS_PATH is not set. ${REMEDY}`);
      }
      try {
        return await browserType.launch(...args);
      } catch (error) {
        if (/Executable doesn't exist|playwright install/i.test(String(error?.message))) {
          throw new Error(`${name}.launch could not start a browser: ${error.message}\n\n${REMEDY}`, { cause: error });
        }
        throw error;
      }
    },
  };
}

export const chromium = guarded(playwrightChromium, "chromium");
export const webkit = guarded(playwrightWebkit, "webkit");
