#!/usr/bin/env node

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { validateGuideSiteLinks, validateLiveGuide } from "./guide-site-validator.mjs";

const fixture = mkdtempSync(path.join(tmpdir(), "tine-guide-site-validator-"));

try {
  writeFileSync(
    path.join(fixture, "intentional-examples.html"),
    '<a class="ref" href="link.html">link</a><a class="tag" href="demo.html">#demo</a>',
  );
  validateGuideSiteLinks(fixture);

  writeFileSync(
    path.join(fixture, "accidental-targets.html"),
    '<a class="ref" href="accidental-ref.html">unregistered page</a><a class="tag" href="accidental-tag.html">#unregistered-tag</a>',
  );
  assert.throws(
    () => validateGuideSiteLinks(fixture),
    (error) => {
      assert.match(error.message, /accidental-targets\.html: missing local target accidental-ref\.html/);
      assert.match(error.message, /accidental-targets\.html: missing local target accidental-tag\.html/);
      return true;
    },
  );

  mkdirSync(path.join(fixture, "app"));
  writeFileSync(path.join(fixture, "index.html"), '<script src="app-redirect.js"></script>');
  writeFileSync(
    path.join(fixture, "app", "index.html"),
    '<head><meta name="tine-published" content="snapshot.json"></head>',
  );
  writeFileSync(
    path.join(fixture, "app", "snapshot.json"),
    JSON.stringify({ name: "Tine Guide", home: "Welcome to Tine", pages: [{ read_only: true }] }),
  );
  validateLiveGuide(fixture);
  writeFileSync(
    path.join(fixture, "app", "snapshot.json"),
    JSON.stringify({ name: "Tine Guide", home: "Welcome to Tine", pages: [{ read_only: false }] }),
  );
  assert.throws(() => validateLiveGuide(fixture), /writable page/);
} finally {
  rmSync(fixture, { recursive: true, force: true });
}

console.log("Guide validators enforce links and the read-only live-app shell.");
