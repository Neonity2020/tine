// Startup notices a fresh E2E profile always raises, and how to clear them.
//
// Every native journey runs against a fresh graph + fresh XDG dirs, so the app
// is always in its first-run state and always announces the Guide. That notice
// is STICKY (`Toasts.tsx`: transient toasts dismiss on any click, sticky ones
// only via the ✕), it renders bottom-right, and it stays up for the whole run.
// Anything a journey clicks in that corner is then intercepted rather than hit:
// GH #164 made the page-properties panel tall enough for its Done button to land
// there, and the click failed with "element click intercepted" while the button
// was on-screen, focused, and perfectly reachable by a user who had dismissed
// the notice. `e2e-query-sheet.mjs` had already hit this and grown its own copy
// of the list; this is the shared front door so the next one does not.
//
// The allowlist is EXACT and deliberately narrow. A journey must keep failing on
// an unexpected toast — a query error, a save failure — because those are real
// findings, and a blanket "dismiss every toast" would swallow them silently.
// When a fourth startup notice appears, add it HERE, not in a journey.
export const STARTUP_NOTICES = [
  "New: in-app Guide — learn Sheets, formulas & queries.",
  "Tine did not close cleanly last time. A privacy-safe diagnostic report is available.",
];

export const STARTUP_NOTICE_PREFIXES = [
  // Carries the renderer name, so it cannot be matched exactly.
  "Software rendering is on (",
];

export function isStartupNotice(message) {
  return STARTUP_NOTICES.includes(message)
    || STARTUP_NOTICE_PREFIXES.some((prefix) => message.startsWith(prefix));
}

/**
 * Dismiss the known first-run notices, leaving every other toast standing.
 *
 * Finds and activates in ONE round trip per notice, then re-queries: the toast
 * stack is a Solid `<For each>` keyed by reference, so a handle taken before a
 * dismissal is detached by the re-render that dismissal causes. Same rule as
 * `lib/e2e-navigation.mjs` — never hold an element handle across a re-rendering
 * list.
 *
 * Returns the messages actually dismissed, so a caller can log what it cleared.
 */
export async function dismissStartupNotices(browser, { limit = 10 } = {}) {
  const dismissed = [];
  for (let attempt = 0; attempt < limit; attempt += 1) {
    const message = await browser.execute((exact, prefixes) => {
      const known = (text) => exact.includes(text) || prefixes.some((p) => text.startsWith(p));
      for (const toast of document.querySelectorAll(".toast")) {
        const text = toast.querySelector(".toast-msg")?.textContent?.trim() ?? "";
        if (!known(text)) continue;
        const close = toast.querySelector(".toast-close");
        if (!close) continue;
        close.click();
        return text;
      }
      return null;
    }, STARTUP_NOTICES, STARTUP_NOTICE_PREFIXES);
    if (message === null) return dismissed;
    await browser.waitUntil(
      () => browser.execute(
        (gone) => ![...document.querySelectorAll(".toast-msg")].some((node) => node.textContent?.trim() === gone),
        message,
      ),
      { timeout: 5_000, timeoutMsg: `the dismissed startup notice remained: ${message}` },
    );
    dismissed.push(message);
  }
  throw new Error(`startup notices kept reappearing after ${limit} dismissals: ${JSON.stringify(dismissed)}`);
}
