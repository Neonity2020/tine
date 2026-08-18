import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { afterEach, describe, expect, it } from "vitest";

const roots: string[] = [];

const policyRounds: number = JSON.parse(
  readFileSync(resolve("scripts/bench-policy.json"), "utf8"),
).reliability.rounds;

function report(fileCount = 1_045, managedColdOpenMs = 2_700) {
  const rounds = (value: number) => Array.from({ length: policyRounds }, () => value);
  const metric = (direct: number, managed: number) => ({
    direct: { rawMedianOfRoundMins: direct, roundSpreadPct: 2, roundMins: rounds(direct) },
    managed: { rawMedianOfRoundMins: managed, roundSpreadPct: 2, roundMins: rounds(managed) },
  });
  const values = {
    coldOpenMs: metric(600, managedColdOpenMs),
    saveMs: metric(600, 800),
    keystrokeP95Ms: metric(1, 2),
    keystrokeScheduleLagP95Ms: metric(1, 2),
  };
  return {
    schemaVersion: 2,
    kind: "storage-mode",
    // Policy-driven: the checker requires reliability.rounds entries, and that
    // knob deliberately tightens for the A/B gate and this one together.
    rounds: Array.from({ length: policyRounds }, () => ({})),
    modes: {
      direct: { metrics: Object.fromEntries(Object.entries(values).map(([name, value]) => [name, value.direct])) },
      managed: { metrics: Object.fromEntries(Object.entries(values).map(([name, value]) => [name, value.managed])) },
    },
    manifest: {
      fixture: { name: "real-corpus storage-mode fixture", graph: `${fileCount} text files`, fileCount },
    },
  };
}

function check(value: ReturnType<typeof report>) {
  const root = mkdtempSync(join(tmpdir(), "tine-storage-policy-"));
  roots.push(root);
  const receipt = join(root, "storage-mode.json");
  writeFileSync(receipt, `${JSON.stringify(value)}\n`);
  return spawnSync(process.execPath, [
    resolve("scripts/check-bench-ab.mjs"),
    "--policy", resolve("scripts/bench-policy.json"),
    "--storage-mode", receipt,
  ], { encoding: "utf8" });
}

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

describe("managed-storage release performance policy", () => {
  it("accepts a real-scale paired receipt inside every standing tripwire", () => {
    const result = check(report());
    expect(result.status, result.stderr).toBe(0);
    expect(result.stdout).toContain("Storage-mode Direct-vs-managed budgets passed");
  });

  it("rejects the small synthetic fixture that previously hid the cold-open gap", () => {
    const result = check(report(120));
    expect(result.status).toBe(1);
    expect(result.stderr).toContain("policy requires at least 1000");
  });

  it("rejects a managed cold-open regression even when the receipt is otherwise complete", () => {
    const result = check(report(1_045, 3_100));
    expect(result.status).toBe(1);
    expect(result.stderr).toContain("managed 3100.0 ms exceeds 3000 ms");
  });
});
