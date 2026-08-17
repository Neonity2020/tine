import { afterEach, describe, expect, it, vi } from "vitest";
import { createStartupRecoveryController, type StartupRecoveryDeps } from "./startupRecovery";
import type { SparseV2CancelResult, StorageTransitionEvent } from "./types";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

async function settle() {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

function fakeCancelResult(): SparseV2CancelResult {
  return {
    status: {
      state: "legacy_default",
      runtime: null,
      can_activate: true,
      can_retry: false,
      can_cancel: false,
      cancel_reason: null,
      binding_generation: 9,
      application_page_admission: { binding_generation: 9, authority: "direct" },
    },
    binding_generation: 9,
    recovery_statement: "Direct Files is active.",
  };
}

function dependencies(overrides: Partial<StartupRecoveryDeps> = {}): StartupRecoveryDeps {
  return {
    lookupGraphPath: vi.fn(async () => "/graphs/alpha"),
    injectedGraphPath: () => "",
    persistedGraphPath: () => "/graphs/alpha",
    openGraph: vi.fn(async () => ({ kind: "loaded" as const, root: "/graphs/alpha" })),
    pickGraph: vi.fn(async () => ({ kind: "loaded" as const, root: "/graphs/beta" })),
    coldReturn: vi.fn(async () => fakeCancelResult()),
    acceptColdReturn: vi.fn(),
    confirmColdReturn: vi.fn(async () => true),
    copyText: vi.fn(async () => {}),
    notify: vi.fn(),
    completeFirstLoad: vi.fn(),
    ...overrides,
  };
}

function transition(overrides: Partial<StorageTransitionEvent> = {}): StorageTransitionEvent {
  return {
    operationId: 1,
    window: "main",
    kind: "open_managed",
    phase: "opening_managed",
    elapsedMs: 10,
    terminal: false,
    ...overrides,
  };
}

afterEach(() => vi.useRealTimers());

describe("native-supervised startup recovery", () => {
  it("does not invent a storage failure when native work is merely slow", async () => {
    vi.useFakeTimers();
    const lookup = deferred<string | null>();
    const controller = createStartupRecoveryController(dependencies({
      lookupGraphPath: vi.fn(() => lookup.promise),
    }));
    controller.start();
    await vi.advanceTimersByTimeAsync(120_000);
    expect(controller.snapshot()).toMatchObject({ mode: "working", operation: "lookup" });
    expect(controller.snapshot().detail).toBeNull();
    controller.dispose();
  });

  it("opens the remembered graph and retires the renderer", async () => {
    const deps = dependencies();
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await settle();
    expect(deps.lookupGraphPath).toHaveBeenCalledWith();
    expect(deps.openGraph).toHaveBeenCalledWith("/graphs/alpha");
    expect(controller.snapshot().mode).toBe("idle");
    controller.dispose();
  });

  it("invalidates a late lookup when another graph is selected", async () => {
    const lookup = deferred<string | null>();
    const deps = dependencies({ lookupGraphPath: vi.fn(() => lookup.promise) });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await controller.openAnother();
    lookup.resolve("/graphs/stale");
    await settle();
    expect(deps.openGraph).not.toHaveBeenCalledWith("/graphs/stale");
    expect(controller.snapshot().mode).toBe("idle");
    controller.dispose();
  });

  it("renders only the newest native operation identity", async () => {
    const lookup = deferred<string | null>();
    const controller = createStartupRecoveryController(dependencies({
      lookupGraphPath: vi.fn(() => lookup.promise),
    }));
    controller.start();
    controller.receiveTransition(transition({ operationId: 4, phase: "opening_managed" }));
    controller.receiveTransition(transition({ operationId: 3, phase: "looking_up_selection" }));
    expect(controller.snapshot()).toMatchObject({
      operationId: 4,
      transitionKind: "open_managed",
      phase: "opening_managed",
    });
    controller.dispose();
  });

  it("uses native-selected emergency return without a frontend authority token", async () => {
    const firstOpen = deferred<Awaited<ReturnType<StartupRecoveryDeps["openGraph"]>>>();
    const openGraph = vi.fn()
      .mockImplementationOnce(() => firstOpen.promise)
      .mockResolvedValueOnce({ kind: "already_current", root: "/graphs/alpha" });
    const coldReturn = vi.fn(async () => fakeCancelResult());
    const deps = dependencies({ openGraph, coldReturn });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await settle();

    await controller.returnToDirectFiles();
    expect(coldReturn).toHaveBeenCalledWith("/graphs/alpha");
    expect(openGraph).toHaveBeenNthCalledWith(2, "/graphs/alpha", true);
    expect(controller.snapshot().mode).toBe("idle");

    firstOpen.resolve({ kind: "already_current", root: "/graphs/alpha" });
    await settle();
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();
    controller.dispose();
  });

  it("keeps stale managed transition events inert after emergency return", async () => {
    const firstOpen = deferred<Awaited<ReturnType<StartupRecoveryDeps["openGraph"]>>>();
    const openGraph = vi.fn()
      .mockImplementationOnce(() => firstOpen.promise)
      .mockResolvedValueOnce({ kind: "already_current", root: "/graphs/alpha" });
    const controller = createStartupRecoveryController(dependencies({ openGraph }));
    controller.start();
    await settle();
    controller.receiveTransition(transition({ operationId: 7 }));
    await controller.returnToDirectFiles();
    controller.receiveTransition(transition({
      operationId: 7,
      phase: "opening_managed",
      terminal: true,
      outcome: "succeeded",
      elapsedMs: 30_000,
    }));
    expect(controller.snapshot().mode).toBe("idle");
    controller.dispose();
  });

  it("reports an actual command refusal and keeps recovery actions available", async () => {
    const deps = dependencies({ openGraph: vi.fn(async () => { throw new Error("missing graph"); }) });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await settle();
    expect(controller.snapshot()).toMatchObject({ mode: "recovery", phase: "graph.failed" });
    expect(controller.snapshot().detail).toBe("The command failed without a safe diagnostic detail.");
    controller.dispose();
  });
});
