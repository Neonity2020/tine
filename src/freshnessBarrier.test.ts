import { afterEach, describe, expect, it, vi } from "vitest";
import {
  beginFreshnessBarrier,
  deferEditorStartUntilFresh,
  endFreshnessBarrier,
  freshnessPending,
} from "./freshnessBarrier";

afterEach(() => endFreshnessBarrier());

describe("focus freshness input barrier", () => {
  it("defers editor activation until fresh state is installed", () => {
    const start = vi.fn();
    beginFreshnessBarrier();
    expect(freshnessPending()).toBe(true);
    expect(deferEditorStartUntilFresh(start)).toBe(true);
    expect(start).not.toHaveBeenCalled();

    endFreshnessBarrier();
    expect(freshnessPending()).toBe(false);
    expect(start).toHaveBeenCalledTimes(1);
  });

  it("keeps only the newest activation attempted during one barrier", () => {
    const stale = vi.fn();
    const latest = vi.fn();
    beginFreshnessBarrier();
    deferEditorStartUntilFresh(stale);
    deferEditorStartUntilFresh(latest);
    endFreshnessBarrier();
    expect(stale).not.toHaveBeenCalled();
    expect(latest).toHaveBeenCalledTimes(1);
  });
});

