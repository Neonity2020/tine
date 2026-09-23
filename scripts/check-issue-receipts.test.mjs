import test from "node:test";
import assert from "node:assert/strict";
import { claimedIssues, hasReceipt } from "./check-issue-receipts.mjs";

const section = [
  "### Added",
  "- Something new (GH #1).",
  "",
  "### Fixed",
  "- A crash on open (GH #408).",
  "- Another thing (GH #451), also touching GH #408.",
  "",
  "### Changed",
  "- Not a fix (GH #999).",
].join("\n");

test("collects the issues a release claims to have FIXED, and only those", () => {
  assert.deepEqual(claimedIssues(section), [408, 451]);
});

test("a release that names no issue claims nothing", () => {
  assert.deepEqual(claimedIssues("### Fixed\n- An internal cleanup.\n"), []);
});

const maintainers = new Set(["martinkoutecky"]);

test("recognises the receipt wording the working agreement asks for", () => {
  for (const body of [
    "Fixed on master; expected in the next release (usually 1-2 days).",
    // House style puts the branch in code: the v0.6.985 close-out missed a real
    // receipt ("Fixed on `master` (`c1b14a85`) ...") until this was matched.
    "Fixed on `master` (`c1b14a85`, integrated at `adb3d362`).",
    "This is now fixed-on-master: the root cause was ...",
    "Closing, should be fixed in v0.6.984; please report back here if not.",
  ]) {
    assert.equal(hasReceipt([{ author: "martinkoutecky", body }], maintainers), true, body);
  }
});

test("a reporter saying it works is not a maintainer receipt", () => {
  assert.equal(
    hasReceipt([{ author: "a-reporter", body: "fixed on master for me too, thanks!" }], maintainers),
    false,
  );
});

test("an issue with no maintainer comment at all is the case this exists to catch", () => {
  assert.equal(hasReceipt([{ author: "a-reporter", body: "still broken on 0.6.983" }], maintainers), false);
  assert.equal(hasReceipt([], maintainers), false);
});

test("a maintainer comment that is not a receipt does not count as one", () => {
  assert.equal(
    hasReceipt([{ author: "martinkoutecky", body: "Thanks for the report — which OS is this?" }], maintainers),
    false,
  );
});
