// The guard the `vite preview` orphan of v0.6.984 should have had.
import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { survivingProcessGroup, reapProcessGroup } from "./lib/e2e-process-group.mjs";

const lines = (...entries) => () => entries;

test("names the processes still in the scenario's group", () => {
  const found = survivingProcessGroup(4242, lines(
    "  4242  4242 node scripts/e2e-thing.mjs",
    "  4300  4242 vite preview --port 5173",
    "  9001  9001 some unrelated process",
  ));
  assert.deepEqual(found.map((entry) => entry.pid), [4242, 4300]);
  assert.match(found[1].args, /vite preview/);
});

test("a reaped-but-unwaited child is not a leak", () => {
  assert.deepEqual(survivingProcessGroup(4242, lines("  4300  4242 [node] <defunct>")), []);
});

test("an empty group reports no leak and kills nothing", async () => {
  let killed = null;
  const survivors = await reapProcessGroup(4242, {
    list: lines("  9001  9001 unrelated"),
    kill: (pgid) => { killed = pgid; },
    attempts: 3,
    intervalMs: 0,
  });
  assert.deepEqual(survivors, []);
  assert.equal(killed, null);
});

test("a group that never empties is reported and killed", async () => {
  let killed = null;
  const survivors = await reapProcessGroup(4242, {
    list: lines("  4300  4242 vite preview --port 5173"),
    kill: (pgid) => { killed = pgid; },
    attempts: 3,
    intervalMs: 0,
  });
  assert.deepEqual(survivors.map((entry) => entry.pid), [4300]);
  assert.equal(killed, 4242);
});

test("teardown lag is not a leak", async () => {
  let remaining = 2;
  const list = () => (remaining-- > 0 ? ["  4300  4242 WebKitWebDriver"] : []);
  const survivors = await reapProcessGroup(4242, { list, kill: () => {}, attempts: 5, intervalMs: 0 });
  assert.deepEqual(survivors, []);
});

// Nothing above touches a real process. This one does, so the detector is
// proven against the operating system and not only against its own fixture.
test("detects a real orphan left behind by a scenario-shaped child", { skip: process.platform === "win32" }, async () => {
  const child = spawn("sh", ["-c", "sleep 30 & exit 0"], { detached: true, stdio: "ignore" });
  await new Promise((resolve) => child.once("exit", resolve));
  const survivors = await reapProcessGroup(child.pid, { attempts: 2, intervalMs: 100 });
  assert.equal(survivors.length, 1, `expected the orphaned sleep, saw ${JSON.stringify(survivors)}`);
  assert.match(survivors[0].args, /sleep 30/);
  // reapProcessGroup killed the group; the orphan must actually be gone.
  await new Promise((resolve) => setTimeout(resolve, 200));
  assert.deepEqual(survivingProcessGroup(child.pid), []);
});

test("a scenario that cleans up after itself reports nothing", { skip: process.platform === "win32" }, async () => {
  const child = spawn("sh", ["-c", "exit 0"], { detached: true, stdio: "ignore" });
  await new Promise((resolve) => child.once("exit", resolve));
  assert.deepEqual(await reapProcessGroup(child.pid, { attempts: 2, intervalMs: 100 }), []);
});
