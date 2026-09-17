// Click a thing, and if something is in the way, SAY WHAT.
//
// WebDriver has two ways of refusing a click and neither names a culprit:
// "element click intercepted" (the driver hit-tested and landed on a different
// node) and "element (...) still not clickable after Nms" (it never landed).
// Both read as "the element is broken"; both actually mean "a click aimed at
// this element would not have arrived". The journey then fails with a stack
// trace whose most specific fact is a WebDriver node id, and finding the cause
// costs a round trip on the machine where it reproduced — a hosted runner,
// usually, because the cause is transient and only a slower machine still
// shows it.
//
// That has now cost this project three times in one release: `pdf-logseq`'s
// block-ref chip was intercepted on the hosted Linux runner in TWO consecutive
// candidate assemblies while passing locally every time, and `print-security`'s
// Search button was "not clickable" on the Windows runner for 20 seconds while
// every local run clicked it. `lib/e2e-toasts.mjs` records the same shape a
// fourth time (GH #164, a sticky first-run notice covering a corner).
//
// So: establish the OBSERVABLE PRECONDITIONS of a click landing, then click.
// A longer timeout or a bare retry is not a precondition; it is a guess about
// how long the obstacle lasts. There are three preconditions, and the second
// candidate assembly is what taught us the second and third:
//
//  1. REACHABLE. The element is the node a click at the driver's aim point
//     would reach. Note *the driver's* aim point: the WebDriver spec aims at
//     the centre of the element's FIRST CSS box intersected with the viewport,
//     not at the centre of `getBoundingClientRect()`. For an inline chip that
//     wraps across two lines those are different points, and the union centre
//     can sit in the gap between the two line boxes. Checking the union centre
//     and reporting "nothing was on top" while the driver refuses the click is
//     exactly what the second failure looked like.
//  2. STILL. The element has stopped moving. The driver scrolls the element
//     into view and then hit-tests it; an element still settling from a layout
//     change is hit-tested where it no longer is. This is why the failure said
//     "nothing was on top when the failure was inspected" — by then it had
//     settled. Motion is best-effort here: it must never be the reason a click
//     fails, only a reason to wait a moment first.
//  3. UNSHADOWED BY THE POINTER'S OWN PAST. A `title` tooltip from the
//     previous click's position is a native window on Linux and Windows; the
//     DOM cannot see it. Parking the pointer first is what a user does anyway
//     on the way to the target.
//
// Then click, and retry a refusal for as long as the deadline allows. This is
// not rerun-to-green: the driver refuses BEFORE dispatching, so the action
// provably did not happen and a retry cannot double-fire it. If the deadline
// passes, fail naming what is at the aim point, which is the one fact the next
// reader needs.
//
// This deliberately does NOT dismiss anything. An unexpected overlay — a query
// error, a save failure — is a real finding, and swallowing it is how a journey
// stops protecting the product. Dismissing the KNOWN first-run notices is a
// separate, exact, allowlisted step: `lib/e2e-toasts.mjs`.

const REFUSED = /element click intercepted|not clickable|not interactable|obscure/i;

// Returns null when a click at the driver's aim point would reach the element
// (or one of its descendants), and otherwise a description of what stands in
// the way, at both the spec's aim point and the bounding-box centre.
//
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

    // The WebDriver in-view centre point: the first CSS box, clipped to the
    // viewport. `getBoundingClientRect()` is the union of every box and is a
    // different point whenever the element wraps.
    const boxes = element.getClientRects();
    const first = boxes.length > 0 ? boxes[0] : element.getBoundingClientRect();
    const left = Math.max(0, first.left);
    const top = Math.max(0, first.top);
    const right = Math.min(window.innerWidth, first.right);
    const bottom = Math.min(window.innerHeight, first.bottom);
    if (right <= left || bottom <= top) {
      return {
        reason: boxes.length === 0 || first.width === 0 || first.height === 0 ? "zero-sized" : "off-viewport",
        firstBox: { left: first.left, top: first.top, width: first.width, height: first.height },
        viewport: { width: window.innerWidth, height: window.innerHeight },
      };
    }
    const aim = { x: (left + right) / 2, y: (top + bottom) / 2 };
    const hit = document.elementFromPoint(aim.x, aim.y);
    if (!hit) return { reason: "nothing-at-aim-point", aim };
    if (hit === element || element.contains(hit) || hit.contains(element)) return null;

    // Report the union centre too: when the two points disagree, the wrapping
    // inline element is the story and the reader needs to see it.
    const union = element.getBoundingClientRect();
    const centre = { x: union.left + union.width / 2, y: union.top + union.height / 2 };
    const atCentre = document.elementFromPoint(centre.x, centre.y);
    return {
      reason: "covered",
      aim,
      by: describe(hit),
      wanted: describe(element),
      boxes: boxes.length,
      centre,
      atCentre: atCentre ? describe(atCentre) : null,
    };
  }, selector);
}

// The element's first box, as the driver sees it. Two identical reads in a row
// mean it has stopped moving.
async function box(browser, selector) {
  return browser.execute((sel) => {
    const element = document.querySelector(sel);
    if (!element) return null;
    const boxes = element.getClientRects();
    const first = boxes.length > 0 ? boxes[0] : element.getBoundingClientRect();
    return [
      Math.round(first.left), Math.round(first.top),
      Math.round(first.width), Math.round(first.height),
    ].join(",");
  }, selector);
}

// Best-effort: give the element up to `budget` to stop moving, then proceed
// regardless. A permanently animating element must not become a test failure —
// only a genuine refusal is.
async function settle(browser, selector, budget) {
  const until = Date.now() + budget;
  let previous = await box(browser, selector);
  while (Date.now() < until) {
    await browser.pause(80);
    const current = await box(browser, selector);
    if (current !== null && current === previous) return;
    previous = current;
  }
}

// Move the pointer out of the way so a tooltip left over from the previous
// interaction cannot sit on the target. Native tooltips are invisible to the
// DOM, so this cannot be checked — only avoided. A driver that rejects the
// action is no reason to fail the journey.
async function parkPointer(browser) {
  try {
    await browser.performActions([{
      type: "pointer",
      id: "e2e-click-park",
      parameters: { pointerType: "mouse" },
      actions: [{ type: "pointerMove", duration: 0, origin: "viewport", x: 2, y: 2 }],
    }]);
    await browser.releaseActions();
  } catch {
    // Not every driver implements Actions; the click below is the real work.
  }
}

/**
 * Click `selector` once a click aimed at it would actually land.
 *
 * `what` names the element for the failure message; default is the selector.
 */
export async function clickWhenReachable(browser, selector, { timeout = 15_000, what = selector } = {}) {
  const deadline = Date.now() + timeout;
  await parkPointer(browser);
  let blocked = await obstruction(browser, selector);
  for (;;) {
    while (blocked !== null && Date.now() < deadline) {
      await browser.pause(100);
      blocked = await obstruction(browser, selector);
    }
    if (blocked !== null) {
      throw new Error(`${what} never became clickable within ${timeout}ms: ${JSON.stringify(blocked)}`);
    }
    await settle(browser, selector, Math.min(2_000, Math.max(0, deadline - Date.now())));
    try {
      await browser.$(selector).click();
      return;
    } catch (error) {
      const late = await obstruction(browser, selector);
      if (!REFUSED.test(String(error.message)) || Date.now() >= deadline) {
        const detail = late === null
          ? "nothing was at the aim point when the failure was inspected"
          : JSON.stringify(late);
        throw new Error(`clicking ${what} failed — ${detail}: ${error.message}`, { cause: error });
      }
      // Refused, not dispatched: safe to aim again.
      blocked = late;
      await browser.pause(150);
    }
  }
}
