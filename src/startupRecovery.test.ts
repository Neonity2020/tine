import { afterEach, describe, expect, it, vi } from "vitest";
import { createStartupRecoveryController, type StartupRecoveryDeps } from "./startupRecovery";

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

function dependencies(overrides: Partial<StartupRecoveryDeps> = {}): StartupRecoveryDeps {
  return {
    lookupGraphPath: vi.fn(async () => "/graphs/alpha"),
    injectedGraphPath: () => "",
    persistedGraphPath: () => "/graphs/alpha",
    openGraph: vi.fn(async () => ({ kind: "loaded" as const, root: "/graphs/alpha" })),
    pickGraph: vi.fn(async () => ({ kind: "loaded" as const, root: "/graphs/beta" })),
    copyText: vi.fn(async () => {}),
    notify: vi.fn(),
    completeFirstLoad: vi.fn(),
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

  it("reports an actual command refusal and keeps recovery actions available", async () => {
    vi.useFakeTimers();
    const deps = dependencies({ openGraph: vi.fn(async () => { throw new Error("missing graph"); }) });
    const controller = createStartupRecoveryController(deps);
    controller.start();
    await settle();
    expect(controller.snapshot()).toMatchObject({ mode: "recovery", phase: "graph.failed" });
    expect(controller.snapshot().detail).toBe("The command failed without a safe diagnostic detail.");
    const terminalElapsed = controller.snapshot().elapsedMs;
    await vi.advanceTimersByTimeAsync(60_000);
    expect(controller.snapshot().elapsedMs).toBe(terminalElapsed);
    controller.dispose();
  });
});
