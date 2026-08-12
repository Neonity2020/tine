import { describe, expect, it, vi } from "vitest";
import { createManagedStorageRuntimeBridge, managedStorageRuntimeErrorMessage } from "./managedStorageRuntime";
import type {
  ManagedApplicationMoveSubtreesRecoveryResult,
  SparseV2RuntimeStatus,
  SparseV2RuntimeStatusEvent,
  SparseV2Status,
  SparseV2Tick,
} from "./types";

function recovery(
  previousBinding: number,
  binding: number,
  episodeId = "019d2e53-3cf0-7a31-a19b-1bdf47b7d3a1",
): ManagedApplicationMoveSubtreesRecoveryResult {
  const status = active(binding);
  return {
    previous_binding_generation: previousBinding,
    binding_generation: binding,
    status,
    application_page_admission: status.application_page_admission,
    episode_id: episodeId,
    outcome: { status: "no_commit", episode_id: episodeId, reason: "episode_not_committed" },
  };
}

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
    application_page_admission: {
      binding_generation: bindingGeneration,
      authority: "managed_writable",
      application_save_page_blocks: 511,
      application_page_request_text_bytes: 1_048_576,
      application_page_max_depth: 128,
    },
  };
}

function runtimeEvent(
  bindingGeneration: number,
  lifecycle: SparseV2RuntimeStatus["lifecycle"],
): SparseV2RuntimeStatusEvent {
  return {
    binding_generation: bindingGeneration,
    runtime: { ...runtime(), lifecycle },
    application_page_admission: lifecycle === "active"
      ? active(bindingGeneration).application_page_admission
      : { binding_generation: bindingGeneration, authority: "managed_unavailable" },
  };
}

describe("managed-storage runtime event bridge", () => {
  it("owns one typed subscription set and carries live runtime state into the current binding", async () => {
    let statusListener: ((event: SparseV2RuntimeStatusEvent) => void) | undefined;
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

    statusListener?.(runtimeEvent(41, "active"));
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

  it("installs the load-route authority atomically and cannot let an old managed transition replace Direct", () => {
    const bridge = createManagedStorageRuntimeBridge({
      sparseV2Status: vi.fn().mockResolvedValue(active(31)),
      onSparseV2Status: async () => () => {},
      onSparseV2Tick: async () => () => {},
      onSparseV2Error: async () => () => {},
    });

    expect(bridge.bind(31, active(31).application_page_admission)).toBe(true);
    expect(bridge.snapshot().applicationPageAdmission).toMatchObject({
      binding_generation: 31,
      authority: "managed_writable",
      application_save_page_blocks: 511,
    });
    expect(bridge.bind(32, { binding_generation: 32, authority: "direct" })).toBe(true);
    expect(bridge.snapshot().applicationPageAdmission).toEqual({
      binding_generation: 32,
      authority: "direct",
    });
    expect(bridge.receiveStatus(active(31))).toBe(false);
    expect(bridge.transitionTo(active(31), 31)).toBe(false);
    expect(bridge.snapshot().applicationPageAdmission).toEqual({
      binding_generation: 32,
      authority: "direct",
    });
  });

  it.each(["stopped_safe", "stopped_crashed", "terminal"] as const)(
    "replaces writable admission with managed-unavailable on a same-generation %s event",
    (lifecycle) => {
      const bridge = createManagedStorageRuntimeBridge({
        sparseV2Status: vi.fn().mockResolvedValue(active(51)),
        onSparseV2Status: async () => () => {},
        onSparseV2Tick: async () => () => {},
        onSparseV2Error: async () => () => {},
      });
      bridge.bind(51, active(51).application_page_admission);

      expect(bridge.receiveRuntimeStatus(runtimeEvent(51, lifecycle))).toBe(true);
      expect(bridge.snapshot().runtime?.lifecycle).toBe(lifecycle);
      expect(bridge.snapshot().applicationPageAdmission).toEqual({
        binding_generation: 51,
        authority: "managed_unavailable",
      });
    },
  );

  it("cannot let a stale runtime event replace a newer Direct or managed binding", () => {
    const bridge = createManagedStorageRuntimeBridge({
      sparseV2Status: vi.fn().mockResolvedValue(active(61)),
      onSparseV2Status: async () => () => {},
      onSparseV2Tick: async () => () => {},
      onSparseV2Error: async () => () => {},
    });
    bridge.bind(61, active(61).application_page_admission);
    bridge.bind(62, { binding_generation: 62, authority: "direct" });

    expect(bridge.receiveRuntimeStatus(runtimeEvent(61, "terminal"))).toBe(false);
    expect(bridge.snapshot().applicationPageAdmission).toEqual({
      binding_generation: 62,
      authority: "direct",
    });

    bridge.bind(63, active(63).application_page_admission);
    expect(bridge.receiveRuntimeStatus(runtimeEvent(61, "active"))).toBe(false);
    expect(bridge.snapshot().applicationPageAdmission).toEqual(active(63).application_page_admission);
  });

  it("accepts a recovery handoff only behind the live episode and page-instance owner", () => {
    const bridge = createManagedStorageRuntimeBridge({
      sparseV2Status: vi.fn().mockResolvedValue(active(71)),
      onSparseV2Status: async () => () => {},
      onSparseV2Tick: async () => () => {},
      onSparseV2Error: async () => () => {},
    });
    const episode = "019d2e53-3cf0-7a31-a19b-1bdf47b7d3a1";
    bridge.bind(71, active(71).application_page_admission);

    expect(bridge.transitionMoveRecovery(recovery(71, 72, episode), episode, 71, () => false)).toBe(false);
    expect(bridge.snapshot().bindingGeneration).toBe(71);

    const mismatchedAdmission = recovery(71, 72, episode);
    mismatchedAdmission.application_page_admission = {
      binding_generation: 72,
      authority: "managed_unavailable",
    };
    expect(bridge.transitionMoveRecovery(mismatchedAdmission, episode, 71, () => true)).toBe(false);
    expect(bridge.snapshot().bindingGeneration).toBe(71);

    const handoff = recovery(71, 72, episode);
    expect(bridge.transitionMoveRecovery(handoff, episode, 71, () => true)).toBe(true);
    expect(bridge.snapshot().bindingGeneration).toBe(72);
    expect(bridge.receiveRuntimeStatus(runtimeEvent(71, "active"))).toBe(false);
    expect(bridge.transitionMoveRecovery(handoff, episode, 71, () => true)).toBe(false);

    bridge.bind(73, active(73).application_page_admission);
    expect(bridge.transitionMoveRecovery(recovery(72, 74, episode), episode, 72, () => true)).toBe(false);
    expect(bridge.snapshot().bindingGeneration).toBe(73);
  });
});
