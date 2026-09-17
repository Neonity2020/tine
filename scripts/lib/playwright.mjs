// Playwright, with a launch failure that names its remedy.
//
// The toolchain lives OUTSIDE the repo in a sibling `.toolchain/`, and
// `scripts/env.sh` is what points `PLAYWRIGHT_BROWSERS_PATH` at it. A script
// run without sourcing env.sh therefore fails inside Playwright with
// "Executable doesn't exist at .../ms-playwright/chromium-.../chrome" and a
// suggestion to run `npx playwright install` — which is the wrong remedy HERE:
// it would download a second copy of a browser this machine already has, into
// a directory nothing reads. The right remedy is one line, and the error should
// say it rather than leave each reader to rediscover it.
//
// It must say it by CATCHING a real failure, never by demanding
// PLAYWRIGHT_BROWSERS_PATH up front. An unset variable is not a fault: on a
// hosted runner `npx playwright install` (ui-e2e.yml) puts the browsers in
// Playwright's own default location and nothing sets that variable at all.
// The first version of this wrapper threw on the unset variable, which turned
// the supported CI configuration into a hard failure — a precondition asserted
// where a property was meant, and the exact shape the positive-property gate
// exists to catch. Locally it cost three journeys of a release E2E suite
// (selection-wrap, publish-security, published-app) before anything launched.
//
// Only `launch` is wrapped, because `launch` is the only member any script in
// this repository uses (`src/playwrightImports.guard.test.ts` keeps that true,
// and keeps new scripts importing this module rather than the package).
import { chromium as playwrightChromium, webkit as playwrightWebkit } from "playwright";

export const REMEDY = "On this machine, run `source scripts/env.sh` first: the browsers live in the sibling "
  + ".toolchain/ms-playwright, outside the repo, and PLAYWRIGHT_BROWSERS_PATH is how Playwright finds "
  + "them. Do NOT run `npx playwright install` here — it downloads a second copy into a directory "
  + "nothing reads. On a hosted runner the install step in .github/workflows/ui-e2e.yml is what "
  + "provides them, and PLAYWRIGHT_BROWSERS_PATH is legitimately unset.";

/**
 * Wrap a browser type so a launch that cannot find a browser names the remedy.
 * Exported so `src/playwrightImports.guard.test.ts` can drive it with a stub
 * instead of a real browser.
 */
export function guarded(browserType, name) {
  return {
    async launch(...args) {
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
