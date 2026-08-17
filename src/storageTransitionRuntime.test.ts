import { describe, expect, it } from "vitest";
import { createStorageTransitionRuntime } from "./storageTransitionRuntime";
import type { StorageTransitionEvent } from "./types";

const transition = (overrides: Partial<StorageTransitionEvent> = {}): StorageTransitionEvent => ({
  operationId: 7,
  window: "main",
  kind: "activate_managed",
  phase: "activating_managed",
  elapsedMs: 12,
  terminal: false,
  ...overrides,
});

describe("storage transition runtime", () => {
  it("renders only the newest native operation and clears busy state on its terminal receipt", () => {
    const runtime = createStorageTransitionRuntime();
    expect(runtime.receive(transition())).toBe(true);
    expect(runtime.active()?.operationId).toBe(7);
    expect(runtime.receive(transition({ operationId: 6 }))).toBe(false);
    expect(runtime.active()?.operationId).toBe(7);
    expect(runtime.receive(transition({ terminal: true, outcome: "succeeded" }))).toBe(true);
    expect(runtime.active()).toBeNull();
  });
});
