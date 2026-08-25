import { beforeEach, expect, it, vi } from "vitest";

vi.mock("./debug", () => ({ dbg: vi.fn() }));

import { dbg } from "./debug";
import {
  beginPageDeleteTrace,
  markPageDeleteFallbackFetch,
  markPageDeleteFallbackFirstPaint,
} from "./pageDeleteTrace";

beforeEach(() => vi.mocked(dbg).mockClear());

it("names every reporter-visible delete boundary without logging graph content", () => {
  const trace = beginPageDeleteTrace("page");
  trace.phase("confirm-accepted");
  trace.phase("dirty-flush-start");
  trace.phase("dirty-flush-complete");
  trace.phase("native-command-start");
  trace.phase("durable-response");
  trace.phase("route-retirement-start");
  trace.armFallback();
  trace.phase("route-retirement-complete");
  const token = markPageDeleteFallbackFetch("main", "journals");
  expect(token).toBe(trace.id);
  markPageDeleteFallbackFirstPaint(token!, "ready");

  const log = vi.mocked(dbg).mock.calls.map(([line]) => line).join("\n");
  for (const phase of [
    "confirm-start",
    "confirm-accepted",
    "dirty-flush-start",
    "dirty-flush-complete",
    "native-command-start",
    "durable-response",
    "route-retirement-start",
    "route-retirement-complete",
    "fallback-fetch-start",
    "fallback-first-paint",
  ]) expect(log).toContain(phase);
  expect(log).not.toContain("private");
  expect(log).not.toContain("/pages/");
});
