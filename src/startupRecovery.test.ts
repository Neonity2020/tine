import { afterEach, describe, expect, it, vi } from "vitest";
import {
  createStartupRecoveryController,
  STARTUP_LOOKUP_WATCHDOG_MS,
  type StartupRecoveryDeps,
} from "./startupRecovery";
import type { SparseV2CancelResult } from "./types";

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
    recovery_statement: "Direct file mode is active.",
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

afterEach(() => {
  vi.useRealTimers();
});

describe("cold-start recovery controller", () => {
  it("turns an unresolved remembered-graph lookup into a persistent recovery state", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(10_000);
    const lookup = deferred<string | null>();
    const deps = dependencies({ lookupGraphPath: vi.fn(() => lookup.promise) });
    const controller = createStartupRecoveryController(deps);

    controller.start();
    await vi.advanceTimersByTimeAsync(STARTUP_LOOKUP_WATCHDOG_MS - 1);
    expect(controller.snapshot().mode).toBe("working");

    await vi.advanceTimersByTimeAsync(1);
    expect(controller.snapshot()).toMatchObject({
      mode: "recovery",
      operation: "lookup",
      phase: "lookup.timeout",
      target: "/graphs/alpha",
    });
    expect(controller.snapshot().detail).toContain("workspace lookup is unresponsive");
    expect(deps.completeFirstLoad).not.toHaveBeenCalled();
    controller.dispose();
  });

  it("allows an untouched late lookup result to continue the normal open", async () => {
    vi.useFakeTimers();
    const lookup = deferred<string | null>();
    const deps = dependencies({ lookupGraphPath: vi.fn(() => lookup.promise) });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await vi.advanceTimersByTimeAsync(STARTUP_LOOKUP_WATCHDOG_MS);
    expect(controller.snapshot().mode).toBe("recovery");

    lookup.resolve("/graphs/late");
    await settle();
    expect(deps.openGraph).toHaveBeenCalledWith("/graphs/late");
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();
    expect(controller.snapshot().mode).toBe("idle");
    controller.dispose();
  });

  it("invalidates a late lookup result when the user chooses another graph", async () => {
    vi.useFakeTimers();
    const lookup = deferred<string | null>();
    const picked = deferred<Awaited<ReturnType<StartupRecoveryDeps["pickGraph"]>>>();
    const deps = dependencies({
      lookupGraphPath: vi.fn(() => lookup.promise),
      pickGraph: vi.fn(() => picked.promise),
    });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await vi.advanceTimersByTimeAsync(STARTUP_LOOKUP_WATCHDOG_MS);

    const choosing = controller.openAnother();
    lookup.resolve("/graphs/stale");
    await settle();
    expect(deps.openGraph).not.toHaveBeenCalledWith("/graphs/stale");

    picked.resolve({ kind: "loaded", root: "/graphs/beta" });
    await choosing;
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();
    controller.dispose();
  });

  it("opens a normal remembered graph immediately without waiting for the watchdog", async () => {
    vi.useFakeTimers();
    const deps = dependencies();
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await settle();

    expect(deps.lookupGraphPath).toHaveBeenCalledWith(1);
    expect(deps.openGraph).toHaveBeenCalledWith("/graphs/alpha");
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();
    expect(vi.getTimerCount()).toBe(0);
    controller.dispose();
  });

  it("re-enables recovery after an unresolved native recovery action", async () => {
    vi.useFakeTimers();
    const lookup = deferred<string | null>();
    const picker = deferred<Awaited<ReturnType<StartupRecoveryDeps["pickGraph"]>>>();
    const deps = dependencies({
      lookupGraphPath: vi.fn(() => lookup.promise),
      pickGraph: vi.fn(() => picker.promise),
      actionWatchdogMs: 500,
    });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await vi.advanceTimersByTimeAsync(STARTUP_LOOKUP_WATCHDOG_MS);
    void controller.openAnother();
    await vi.advanceTimersByTimeAsync(500);

    expect(controller.snapshot()).toMatchObject({
      mode: "recovery",
      phase: "native.unavailable",
    });
    expect(controller.snapshot().detail).toContain("picker is unavailable");
    controller.retry();
    expect(deps.lookupGraphPath).toHaveBeenCalledTimes(2);
    controller.dispose();
  });

  it("invalidates the UI continuation but retains the authorized native lookup token for cold Return", async () => {
    vi.useFakeTimers();
    const lookup = deferred<string | null>();
    const coldReturn = vi.fn(async () => fakeCancelResult());
    const deps = dependencies({
      lookupGraphPath: vi.fn(() => lookup.promise),
      coldReturn,
    });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await vi.advanceTimersByTimeAsync(STARTUP_LOOKUP_WATCHDOG_MS);
    expect(controller.snapshot()).toMatchObject({ attempt: 1, nativeAttempt: 1 });

    await controller.returnToDirectFiles();
    expect(coldReturn).toHaveBeenCalledWith("/graphs/alpha", 1);
    expect(controller.snapshot()).toMatchObject({ mode: "idle", attempt: 2 });

    lookup.resolve("/graphs/stale");
    await settle();
    expect(deps.openGraph).not.toHaveBeenCalledWith("/graphs/stale");
    controller.dispose();
  });

  it("recovers from a stuck native confirmation and ignores its late answer after another action", async () => {
    vi.useFakeTimers();
    const lookup = deferred<string | null>();
    const confirmation = deferred<boolean>();
    const coldReturn = vi.fn(async () => fakeCancelResult());
    const deps = dependencies({
      lookupGraphPath: vi.fn(() => lookup.promise),
      confirmColdReturn: vi.fn(() => confirmation.promise),
      coldReturn,
      actionWatchdogMs: 500,
    });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await vi.advanceTimersByTimeAsync(STARTUP_LOOKUP_WATCHDOG_MS);

    void controller.returnToDirectFiles();
    expect(controller.snapshot()).toMatchObject({ mode: "working", phase: "direct.confirm" });
    await vi.advanceTimersByTimeAsync(500);
    expect(controller.snapshot()).toMatchObject({ mode: "recovery", phase: "native.unavailable" });
    expect(controller.snapshot().detail).toContain("confirmation is unavailable");

    controller.retry();
    confirmation.resolve(true);
    await settle();
    expect(coldReturn).not.toHaveBeenCalled();
    expect(deps.lookupGraphPath).toHaveBeenCalledTimes(2);
    controller.dispose();
  });

  it("keeps cold Return callable while managed-open heartbeats repeatedly defer the inactivity watchdog", async () => {
    vi.useFakeTimers();
    const firstOpen = deferred<Awaited<ReturnType<StartupRecoveryDeps["openGraph"]>>>();
    let openCount = 0;
    const openGraph = vi.fn(() => {
      openCount++;
      return openCount === 1
        ? firstOpen.promise
        : Promise.resolve({ kind: "already_current" as const, root: "/graphs/alpha" });
    });
    const coldReturn = vi.fn(async () => fakeCancelResult());
    const deps = dependencies({ openGraph, coldReturn, actionWatchdogMs: 500 });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await settle();
    expect(controller.snapshot()).toMatchObject({
      mode: "working",
      operation: "graph_open",
      nativeAttempt: 1,
    });

    for (let heartbeat = 0; heartbeat < 4; heartbeat++) {
      await vi.advanceTimersByTimeAsync(400);
      controller.receiveProgress({
        phase: "managed_open.waiting_recovering_promoted_runtime",
        elapsed_ms: (heartbeat + 1) * 400,
        terminal: false,
      });
    }
    expect(controller.snapshot()).toMatchObject({ mode: "working", operation: "graph_open" });

    await controller.returnToDirectFiles();
    expect(coldReturn).toHaveBeenCalledWith("/graphs/alpha", 1);
    expect(openGraph).toHaveBeenNthCalledWith(2, "/graphs/alpha", true);
    expect(controller.snapshot().mode).toBe("idle");

    firstOpen.resolve({ kind: "loaded", root: "/graphs/alpha" });
    await settle();
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();
    controller.dispose();
  });

  it("ignores superseded managed-open progress and keeps a live cold return out of recovery", async () => {
    vi.useFakeTimers();
    const firstOpen = deferred<Awaited<ReturnType<StartupRecoveryDeps["openGraph"]>>>();
    const returnResult = deferred<SparseV2CancelResult>();
    const openGraph = vi.fn()
      .mockImplementationOnce(() => firstOpen.promise)
      .mockResolvedValueOnce({ kind: "already_current", root: "/graphs/alpha" });
    const deps = dependencies({
      openGraph,
      coldReturn: vi.fn(() => returnResult.promise),
      actionWatchdogMs: 500,
    });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await settle();

    const returning = controller.returnToDirectFiles();
    await settle();
    expect(controller.snapshot()).toMatchObject({
      mode: "working",
      operation: "cold_return",
      phase: "direct.archive",
      nativePhase: null,
    });

    controller.receiveProgress({
      phase: "managed_open.complete",
      elapsed_ms: 30_059,
      terminal: true,
      outcome: "ok",
    });
    expect(controller.snapshot()).toMatchObject({
      mode: "working",
      operation: "cold_return",
      nativePhase: null,
    });

    for (let heartbeat = 0; heartbeat < 3; heartbeat++) {
      await vi.advanceTimersByTimeAsync(400);
      controller.receiveProgress({
        phase: "cold_return.archiving_managed_state",
        elapsed_ms: (heartbeat + 1) * 400,
        terminal: false,
      });
    }
    expect(controller.snapshot()).toMatchObject({
      mode: "working",
      operation: "cold_return",
      nativePhase: "cold_return.archiving_managed_state",
    });

    returnResult.resolve(fakeCancelResult());
    await returning;
    expect(openGraph).toHaveBeenNthCalledWith(2, "/graphs/alpha", true);
    expect(controller.snapshot().mode).toBe("idle");
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();

    firstOpen.resolve({ kind: "loaded", root: "/graphs/alpha" });
    await settle();
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();
    controller.dispose();
  });

  it("restarts an invalidated managed open when cold Return confirmation is declined", async () => {
    vi.useFakeTimers();
    const firstOpen = deferred<Awaited<ReturnType<StartupRecoveryDeps["openGraph"]>>>();
    const replacementOpen = deferred<Awaited<ReturnType<StartupRecoveryDeps["openGraph"]>>>();
    const confirmation = deferred<boolean>();
    const openGraph = vi.fn()
      .mockImplementationOnce(() => firstOpen.promise)
      .mockImplementationOnce(() => replacementOpen.promise);
    const deps = dependencies({
      openGraph,
      confirmColdReturn: vi.fn(() => confirmation.promise),
    });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await settle();
    expect(controller.snapshot()).toMatchObject({
      mode: "working",
      operation: "graph_open",
      attempt: 1,
    });

    const returning = controller.returnToDirectFiles();
    confirmation.resolve(false);
    await settle();
    expect(openGraph).toHaveBeenCalledTimes(2);
    expect(controller.snapshot()).toMatchObject({
      mode: "working",
      operation: "graph_open",
      attempt: 2,
    });

    firstOpen.resolve({ kind: "loaded", root: "/graphs/alpha" });
    await settle();
    expect(deps.completeFirstLoad).not.toHaveBeenCalled();

    replacementOpen.resolve({ kind: "already_current", root: "/graphs/alpha" });
    await returning;
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();
    expect(controller.snapshot().mode).toBe("idle");
    expect(vi.getTimerCount()).toBe(0);
    controller.dispose();
  });

  it("restarts an invalidated managed open when a timed-out confirmation later declines", async () => {
    vi.useFakeTimers();
    const firstOpen = deferred<Awaited<ReturnType<StartupRecoveryDeps["openGraph"]>>>();
    const confirmation = deferred<boolean>();
    const openGraph = vi.fn()
      .mockImplementationOnce(() => firstOpen.promise)
      .mockResolvedValueOnce({ kind: "already_current", root: "/graphs/alpha" });
    const deps = dependencies({
      openGraph,
      confirmColdReturn: vi.fn(() => confirmation.promise),
      actionWatchdogMs: 500,
    });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await settle();

    const returning = controller.returnToDirectFiles();
    await vi.advanceTimersByTimeAsync(500);
    expect(controller.snapshot()).toMatchObject({ mode: "recovery", phase: "native.unavailable" });

    confirmation.resolve(false);
    await returning;
    expect(openGraph).toHaveBeenCalledTimes(2);
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();
    expect(controller.snapshot().mode).toBe("idle");
    expect(vi.getTimerCount()).toBe(0);

    firstOpen.resolve({ kind: "loaded", root: "/graphs/alpha" });
    await settle();
    expect(deps.completeFirstLoad).toHaveBeenCalledOnce();
    controller.dispose();
  });

  it("uses the privacy-safe native phase vocabulary without accepting it as an outcome", async () => {
    vi.useFakeTimers();
    const lookup = deferred<string | null>();
    const deps = dependencies({ lookupGraphPath: vi.fn(() => lookup.promise) });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    controller.receiveProgress({
      phase: "lookup.settings_read",
      elapsed_ms: 17,
      terminal: false,
    });

    expect(controller.snapshot()).toMatchObject({
      mode: "working",
      nativePhase: "lookup.settings_read",
      nativeElapsedMs: 17,
    });
    expect(deps.openGraph).not.toHaveBeenCalled();
    controller.receiveProgress({
      phase: "managed_open./home/martin/private" as `managed_open.${string}`,
      elapsed_ms: Number.MAX_SAFE_INTEGER,
      terminal: false,
    });
    expect(controller.snapshot().nativePhase).toBe("lookup.settings_read");
    controller.dispose();
  });
});
