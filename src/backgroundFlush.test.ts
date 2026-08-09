import { afterEach, describe, expect, it, vi } from "vitest";
import { installBackgroundFlush, type BackgroundFlushDeps } from "./backgroundFlush";

type Handler = (event: Event) => void;

let hidden = true;

function harness(overrides: Partial<BackgroundFlushDeps> = {}) {
  const listeners = new Map<string, Set<Handler>>();
  const flushAll = vi.fn(async () => true);
  const endEdit = vi.fn();
  const deps: BackgroundFlushDeps = {
    endEdit,
    flushAll,
    closeInFlight: () => false,
    isHidden: () => hidden,
    addEventListener: ((type: string, handler: Handler) => {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type)!.add(handler);
    }) as unknown as typeof document.addEventListener,
    removeEventListener: ((type: string, handler: Handler) => {
      listeners.get(type)?.delete(handler);
    }) as unknown as typeof document.removeEventListener,
    ...overrides,
  };
  const dispose = installBackgroundFlush(deps);
  const fire = (type: string) => {
    for (const handler of [...(listeners.get(type) ?? [])]) handler(new Event(type));
  };
  return { fire, flushAll, endEdit, dispose, listeners };
}

function setVisibility(state: "hidden" | "visible") {
  hidden = state === "hidden";
}

afterEach(() => {
  setVisibility("visible");
  vi.restoreAllMocks();
});

// GH #255 ("Notes are randomly lost on Android and Windows 11"). Before this,
// the only durability barrier in the whole app was a clean desktop window close.
// Android/iOS never send one — the OS backgrounds the app and reclaims it later.
describe("background flush", () => {
  it("writes pending edits when the app is hidden", async () => {
    setVisibility("hidden");
    const { fire, flushAll, endEdit } = harness();
    fire("visibilitychange");
    expect(endEdit).toHaveBeenCalled();
    expect(flushAll).toHaveBeenCalledTimes(1);
  });

  it("commits the in-flight keystroke BEFORE flushing", () => {
    // Otherwise the character being typed at the moment of backgrounding is the
    // one thing that still gets lost.
    setVisibility("hidden");
    const order: string[] = [];
    const { fire } = harness({
      endEdit: () => void order.push("endEdit"),
      flushAll: async () => { order.push("flushAll"); return true; },
    });
    fire("visibilitychange");
    expect(order).toEqual(["endEdit", "flushAll"]);
  });

  it("also fires on pagehide and freeze", () => {
    // Android and iOS WebViews are inconsistent about which teardown event they
    // deliver, so all three are wired and the in-flight guard dedupes.
    setVisibility("hidden");
    for (const event of ["pagehide", "freeze"]) {
      const { fire, flushAll } = harness();
      fire(event);
      expect(flushAll, event).toHaveBeenCalledTimes(1);
    }
  });

  it("does NOT flush when the app becomes visible", () => {
    // visibilitychange fires on both edges; only hiding is a durability event.
    setVisibility("visible");
    const { fire, flushAll } = harness();
    fire("visibilitychange");
    expect(flushAll).not.toHaveBeenCalled();
  });

  it("yields to a close transaction instead of racing it", () => {
    // The close path can prompt the user about an unsaved page; a second
    // concurrent flushAll would resolve against a half-drained queue.
    setVisibility("hidden");
    const { fire, flushAll } = harness({ closeInFlight: () => true });
    fire("visibilitychange");
    expect(flushAll).not.toHaveBeenCalled();
  });

  it("does not stack overlapping flushes", () => {
    setVisibility("hidden");
    let resolve!: (ok: boolean) => void;
    // NB: capture the spy locally — `harness` returns its own default flushAll,
    // not the override.
    const flushAll = vi.fn(() => new Promise<boolean>((done) => { resolve = done; }));
    const { fire } = harness({ flushAll });
    fire("visibilitychange");
    fire("pagehide");
    expect(flushAll).toHaveBeenCalledTimes(1);
    resolve(true);
  });

  it("survives a flush that rejects", async () => {
    // A rejected flush must not leave the guard latched, or the app never
    // flushes again for the rest of its life.
    setVisibility("hidden");
    const flushAll = vi.fn(async () => { throw new Error("disk on fire"); });
    const { fire } = harness({ flushAll });
    fire("visibilitychange");
    await Promise.resolve();
    await Promise.resolve();
    fire("visibilitychange");
    expect(flushAll).toHaveBeenCalledTimes(2);
  });

  it("unregisters every listener on dispose", () => {
    const { dispose, listeners } = harness();
    dispose();
    expect([...listeners.values()].every((set) => set.size === 0)).toBe(true);
  });
});
