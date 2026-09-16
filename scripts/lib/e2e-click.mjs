// Click a thing, and if something is in the way, SAY WHAT.
//
// WebDriver has two ways of refusing a click and neither names a culprit:
// "element click intercepted" (the driver hit-tested and landed on a different
// node) and "element (...) still not clickable after Nms" (it never landed).
// Both read as "the element is broken"; both actually mean "something else is
// on top of it". The journey then fails with a stack trace whose most specific
// fact is a WebDriver node id, and finding the overlay costs a round trip on
// the machine where it reproduced — a hosted runner, usually, because the
// overlay is transient and only a slower machine is still showing it.
//
// That has now cost this project twice in one release: `pdf-logseq`'s block-ref
// chip was intercepted on the hosted Linux runner while passing locally, and
// `print-security`'s Search button was "not clickable" on the Windows runner
// for 20 seconds while every local run clicked it. `lib/e2e-toasts.mjs` records
// the same shape a third time (GH #164, a sticky first-run notice covering the
// bottom-right corner).
//
// So: wait for the OBSERVABLE PRECONDITION — this element is the node a click
// at its centre would actually reach — and then click. A fixed retry or a
// longer timeout is not that precondition; it is a guess about how long the
// overlay lasts. If the precondition never holds, fail naming the node that is
// on top, its classes and its text, which is the one fact the next reader needs.
//
// This deliberately does NOT dismiss anything. An unexpected overlay — a query
// error, a save failure — is a real finding, and swallowing it is how a journey
// stops protecting the product. Dismissing the KNOWN first-run notices is a
// separate, exact, allowlisted step: `lib/e2e-toasts.mjs`.

// Returns null when a click at the element's centre would reach it (or one of
// its descendants), and otherwise a description of what stands in the way.
// The describe step is inlined rather than passed in: the app's webview runs
// under its own CSP, and a helper smuggled across as source would be the one
// part of this file that fails for a reason unrelated to the journey.
async function obstruction(browser, selector) {
  return browser.execute((sel) => {
    const describe = (node) => {
      const classes = typeof node.className === "string" ? node.className.trim() : "";
      return {
        node: node.tagName.toLowerCase() + (classes ? `.${classes.split(/\s+/).join(".")}` : ""),
        text: node.textContent?.trim().slice(0, 120) ?? "",
      };
    };
    const element = document.querySelector(sel);
    if (!element) return { reason: "missing" };
    const rect = element.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) {
      return { reason: "zero-sized", rect: { width: rect.width, height: rect.height } };
    }
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    if (x < 0 || y < 0 || x > window.innerWidth || y > window.innerHeight) {
      return {
        reason: "off-viewport",
        centre: { x, y },
        viewport: { width: window.innerWidth, height: window.innerHeight },
      };
    }
    const hit = document.elementFromPoint(x, y);
    if (!hit) return { reason: "nothing-at-centre", centre: { x, y } };
    if (hit === element || element.contains(hit) || hit.contains(element)) return null;
    return { reason: "covered", centre: { x, y }, by: describe(hit), wanted: describe(element) };
  }, selector);
}

/**
 * Click `selector` once it is genuinely the thing a click would reach.
 *
 * `what` names the element for the failure message; default is the selector.
 */
export async function clickWhenReachable(browser, selector, { timeout = 15_000, what = selector } = {}) {
  const deadline = Date.now() + timeout;
  let blocked = await obstruction(browser, selector);
  while (blocked !== null && Date.now() < deadline) {
    await browser.pause(100);
    blocked = await obstruction(browser, selector);
  }
  if (blocked !== null) {
    throw new Error(`${what} never became clickable within ${timeout}ms: ${JSON.stringify(blocked)}`);
  }
  try {
    await browser.$(selector).click();
  } catch (error) {
    // The overlay can arrive in the gap between the check and the click; report
    // it the same way rather than as a bare WebDriver message.
    const late = await obstruction(browser, selector);
    const detail = late === null ? "nothing was on top when the failure was inspected" : JSON.stringify(late);
    throw new Error(`clicking ${what} failed — ${detail}: ${error.message}`, { cause: error });
  }
}
