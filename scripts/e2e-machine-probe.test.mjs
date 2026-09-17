import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { describeMachine, machineSnapshot, measureFilesystemLatency } from "./lib/e2e-machine-probe.mjs";

test("measures the write-fsync-rename-read cycle and leaves nothing behind", () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "e2e-machine-probe-"));
  const before = fs.readdirSync(directory);
  const measured = measureFilesystemLatency(directory, 3);
  assert.equal(measured.samples, 3);
  assert.ok(measured.medianMs >= 0, `median was ${measured.medianMs}`);
  assert.ok(measured.maxMs >= measured.medianMs);
  assert.deepEqual(fs.readdirSync(directory), before, "the probe left a file in the directory it measured");
  fs.rmSync(directory, { recursive: true, force: true });
});

test("an unwritable directory is reported, never thrown", () => {
  const measured = measureFilesystemLatency(path.join(os.tmpdir(), "e2e-machine-probe-absent", "deeper"), 2);
  assert.equal(measured.samples, 0);
  assert.match(measured.error, /ENOENT/);
});

test("a snapshot carries what a reader needs to place the blame", () => {
  const snapshot = machineSnapshot(os.tmpdir());
  assert.ok(snapshot.cpus > 0);
  assert.ok(Number.isFinite(snapshot.loadAverage1));
  assert.ok(Number.isFinite(snapshot.freeMemoryMb));
  assert.equal(snapshot.filesystem.error, undefined);
  assert.equal(snapshot.tmpFilesystem.error, undefined);
});

test("a stall on the temporary-graph mount alone is still a starved machine", () => {
  const verdict = describeMachine({
    filesystem: { samples: 15, medianMs: 0.6, maxMs: 2 },
    tmpFilesystem: { samples: 15, medianMs: 280, maxMs: 3100 },
    loadAverage1: 1.2,
    cpus: 12,
  });
  assert.match(verdict, /machine was starved/);
});

test("a starved disk is named as such", () => {
  const verdict = describeMachine({ filesystem: { samples: 15, medianMs: 340, maxMs: 4200 }, loadAverage1: 1.1, cpus: 12 });
  assert.match(verdict, /machine was starved/);
  assert.match(verdict, /re-run before reading the diff/);
});

test("a saturated cpu is named even when the disk is fast", () => {
  const verdict = describeMachine({ filesystem: { samples: 15, medianMs: 0.7, maxMs: 3 }, loadAverage1: 31, cpus: 12 });
  assert.match(verdict, /machine was starved/);
});

test("a healthy machine hands the failure back to the product", () => {
  const verdict = describeMachine({ filesystem: { samples: 15, medianMs: 0.5, maxMs: 2.8 }, loadAverage1: 2.1, cpus: 12 });
  assert.match(verdict, /the product, not the machine/);
});

test("a probe that could not run says so rather than exonerating anyone", () => {
  const verdict = describeMachine({ filesystem: { samples: 0, error: "EACCES: permission denied" }, loadAverage1: 1, cpus: 8 });
  assert.match(verdict, /probe itself failed/);
  assert.doesNotMatch(verdict, /the product, not the machine/);
});
