import { afterEach, expect, it, vi } from "vitest";
import { __setBackendForTest, backend } from "./backend";
import { bumpDataRev, dataRev, pageInventoryRev } from "./ui";

const native = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: native.invoke, convertFileSrc: (p: string) => p }));
vi.mock("@tauri-apps/api/event", () => ({ listen: native.listen }));

afterEach(() => {
  __setBackendForTest(null);
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  vi.clearAllMocks();
});

it("reruns on committed images for the current binding without reloading live pages", async () => {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  __setBackendForTest(null);
  const stop = vi.fn();
  let deliver!: (event: { payload: number }) => void;
  native.listen.mockImplementation(async (name: string, cb: typeof deliver) => {
    expect(name).toBe("query-projection-changed");
    deliver = cb;
    return stop;
  });
  native.invoke.mockResolvedValueOnce({ kind: "opened", binding_generation: 41 });
  const api = backend();
  await api.loadGraph("/fixture/a");
  const revision = dataRev();
  const inventory = pageInventoryRev();
  const unlisten = await api.onQueryProjectionChanged(bumpDataRev);
  deliver({ payload: 40 });
  expect(dataRev()).toBe(revision);
  deliver({ payload: 41 });
  expect(dataRev()).toBe(revision + 1);
  expect(pageInventoryRev()).toBe(inventory);
  expect(native.invoke).toHaveBeenCalledTimes(1);

  native.invoke.mockResolvedValueOnce({ kind: "opened", binding_generation: 42 });
  await api.loadGraph("/fixture/b");
  deliver({ payload: 41 });
  expect(dataRev()).toBe(revision + 1);
  deliver({ payload: 42 });
  expect(dataRev()).toBe(revision + 2);
  expect(native.invoke).toHaveBeenCalledTimes(2);
  unlisten();
  expect(stop).toHaveBeenCalledOnce();
});
