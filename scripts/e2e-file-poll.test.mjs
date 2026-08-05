import assert from "node:assert/strict";
import test from "node:test";

import { waitForFileText } from "./e2e-file-poll.mjs";

function transientMissingFile() {
  const error = new Error("temporarily replaced by atomic save");
  error.code = "ENOENT";
  return error;
}

function deterministicClock() {
  let time = 0;
  return {
    now: () => time,
    delay: async () => { time += 1; },
  };
}

test("waitForFileText retries a transient ENOENT from atomic replacement", async () => {
  const clock = deterministicClock();
  let attempts = 0;
  const text = await waitForFileText("fixture.md", (value) => value === "saved", "atomic save", {
    timeoutMs: 3,
    now: clock.now,
    delay: clock.delay,
    readFile: () => {
      attempts += 1;
      if (attempts === 1) throw transientMissingFile();
      return "saved";
    },
  });
  assert.equal(text, "saved");
  assert.equal(attempts, 2);
});

test("waitForFileText fails closed when the file stays absent", async () => {
  const clock = deterministicClock();
  await assert.rejects(
    waitForFileText("fixture.md", () => true, "persistent absence", {
      timeoutMs: 3,
      now: clock.now,
      delay: clock.delay,
      readFile: () => { throw transientMissingFile(); },
    }),
    /persistent absence was not persisted/,
  );
});

test("waitForFileText fails immediately for non-ENOENT read errors", async () => {
  const clock = deterministicClock();
  const denied = Object.assign(new Error("permission denied"), { code: "EACCES" });
  await assert.rejects(
    waitForFileText("fixture.md", () => true, "permission error", {
      timeoutMs: 3,
      now: clock.now,
      delay: clock.delay,
      readFile: () => { throw denied; },
    }),
    (error) => error === denied,
  );
});
