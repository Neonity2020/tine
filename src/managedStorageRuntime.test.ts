import { describe, expect, it, vi } from "vitest";
import { createManagedStorageRuntimeBridge, managedStorageRuntimeErrorMessage } from "./managedStorageRuntime";
import type { SparseV2RuntimeStatus, SparseV2Status, SparseV2Tick } from "./types";

function runtime(lastTick: SparseV2Tick | null = null): SparseV2RuntimeStatus {
  return {
    lifecycle: "active",
    recovery: "first_promotion",
    watcher: {
      latest_enqueue: 4,
      acknowledged: 4,
      drain_in_flight: false,
      pending: false,
      pending_requires_full_scan: false,
      deferred: false,
      quiescing: false,
      sequence_exhausted: false,
    },
    last_tick: lastTick,
    detail: null,
    shared_role: null,
    shared_phase: null,
    provider_pending: 0,
  };
}

function active(bindingGeneration: number): SparseV2Status {
  return {
    state: "active",
    runtime: runtime(),
    can_activate: false,
    can_retry: false,
    can_cancel: true,
    cancel_reason: null,
    binding_generation: bindingGeneration,
  };
}

describe("managed-storage runtime event bridge", () => {
  it("owns one typed subscription set and carries live runtime state into the current binding", async () => {
    let statusListener: ((event: { binding_generation: number; runtime: SparseV2RuntimeStatus }) => void) | undefined;
    let tickListener: ((event: { binding_generation: number; tick: SparseV2Tick }) => void) | undefined;
    let errorListener: ((event: { binding_generation: number; message: string }) => void) | undefined;
    const unlisten = [vi.fn(), vi.fn(), vi.fn()];
    const bridge = createManagedStorageRuntimeBridge({
      sparseV2Status: vi.fn().mockResolvedValue(active(41)),
      onSparseV2Status: vi.fn().mockImplementation(async (listener) => {
        statusListener = listener;
        return unlisten[0];
      }),
      onSparseV2Tick: vi.fn().mockImplementation(async (listener) => {
        tickListener = listener;
        return unlisten[1];
      }),
      onSparseV2Error: vi.fn().mockImplementation(async (listener) => {
        errorListener = listener;
        return unlisten[2];
      }),
    });

    bridge.bind(41);
    const stop = await bridge.listen();
    expect(statusListener).toBeTypeOf("function");
    expect(tickListener).toBeTypeOf("function");
    expect(errorListener).toBeTypeOf("function");

    statusListener?.({ binding_generation: 41, runtime: runtime() });
    const blocked = { state: "blocked", detail: "the exact recovery proof is unavailable", epoch: null };
    tickListener?.({ binding_generation: 41, tick: blocked });
    errorListener?.({ binding_generation: 41, message: "Blocked(\"the exact recovery proof is unavailable\")" });

    expect(bridge.snapshot().runtime?.last_tick).toEqual(blocked);
    expect(bridge.snapshot().tick).toEqual(blocked);
    expect(bridge.snapshot().error).toBe('Blocked("the exact recovery proof is unavailable")');
    expect(managedStorageRuntimeErrorMessage(bridge.snapshot().error!)).toContain(
      'Blocked("the exact recovery proof is unavailable")'
    );

    stop();
    for (const dispose of unlisten) expect(dispose).toHaveBeenCalledOnce();
  });

  it("drops events and late status reads from a graph binding that has been replaced", async () => {
    let resolveStatus: ((status: SparseV2Status) => void) | undefined;
    const bridge = createManagedStorageRuntimeBridge({
      sparseV2Status: () => new Promise<SparseV2Status>((resolve) => { resolveStatus = resolve; }),
      onSparseV2Status: async () => () => {},
      onSparseV2Tick: async () => () => {},
      onSparseV2Error: async () => () => {},
    });

    bridge.bind(7);
    const pending = bridge.refresh();
    bridge.bind(8);
    resolveStatus?.(active(7));

    expect(await pending).toBeNull();
    expect(bridge.snapshot()).toMatchObject({ bindingGeneration: 8, status: null, tick: null, error: null });
    expect(bridge.receiveError({ binding_generation: 7, message: "old graph failure" })).toBe(false);
    expect(bridge.receiveTick({ binding_generation: 7, tick: { state: "failed", detail: "old", epoch: null } })).toBe(false);
    expect(bridge.snapshot().error).toBeNull();

    expect(bridge.receiveStatus(active(8))).toBe(true);
    expect(bridge.receiveError({ binding_generation: 8, message: "current graph failure" })).toBe(true);
    expect(bridge.snapshot().error).toBe("current graph failure");
  });
});
