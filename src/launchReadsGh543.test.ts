// GH #543 (audit R10-06, R10-09): reads the launch pass must not multiply.
// A save made while the index is still coming leaves no extra whole-graph
// read behind it, at most one alias read is in flight, and a graph the backend
// reopened re-reads its navigation index at once.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { __setBackendForTest, backend } from "./backend";
import { mockBackend } from "./mock";
import { bumpDataRev, bumpGraphEpoch } from "./ui";
import type { WarmCacheWaitDeps } from "./warmCache";

// One shared warm gate for every waitForWarmCache: pending until a test says
// the launch check has landed, as the single warm-cache-done does.
let warm: Promise<boolean> = new Promise<boolean>(() => {});
vi.mock("./warmCache", async (orig) => {
  const actual = await orig<typeof import("./warmCache")>();
  return {
    ...actual,
    waitForWarmCache: vi.fn((epoch?: number, deps?: WarmCacheWaitDeps) =>
      deps ? actual.waitForWarmCache(epoch, deps) : warm),
  };
});

const settle = async (n = 20) => { for (let i = 0; i < n; i++) await new Promise((r) => setTimeout(r, 0)); };

afterEach(() => vi.restoreAllMocks());

describe("GH #543 launch-pass reads", () => {
  // R10-09 / R11-09: saves made while a counts read is still out (the index
  // is being built and the backend waits) must not leave one more read each.
  // The read is asked at open, with no wait for the launch check: during the
  // check the backend answers from the stored index (GH #550, launch design
  // D4). A burst of saves costs the read in flight plus one for the newest
  // revision.
  it("keeps one block_ref_counts in flight and one more for a burst of saves", async () => {
    let answer: () => void = () => {};
    const counts = vi.spyOn(backend(), "getBlockRefCounts").mockImplementation(
      () => new Promise((resolve) => { answer = () => resolve({}); }),
    );
    bumpGraphEpoch(); // graph open
    await import("./blockRefCounts");
    await settle();
    expect(counts).toHaveBeenCalledTimes(1); // asked at open, not after the check
    for (let i = 0; i < 8; i++) { bumpDataRev(); await settle(2); } // saves while it is out
    expect(counts).toHaveBeenCalledTimes(1);
    answer();
    await settle();
    expect(counts).toHaveBeenCalledTimes(2);
    answer();
    await settle();
    expect(counts).toHaveBeenCalledTimes(2);
  });

  // R10-09: page_aliases is requested on EVERY dataRev (App.tsx:1387) with no
  // warm wait and no coalescing; during the pass each one blocks a native
  // blocking thread in with_pages and they all answer (each a whole-graph
  // walk, or an index aggregate) when the pass ends.
  it("keeps at most one page_aliases in flight while the index is coming", async () => {
    const api = mockBackend();
    let inFlight = 0;
    let peak = 0;
    const gate = new Promise<void>(() => {}); // the pass never ends in this probe
    vi.spyOn(api, "pageAliases").mockImplementation((async () => {
      inFlight++; peak = Math.max(peak, inFlight);
      await gate;
      return [];
    }) as never);
    __setBackendForTest(api);
    const { refreshAliases } = await import("./graph");
    bumpGraphEpoch();
    for (let i = 0; i < 6; i++) { bumpDataRev(); void refreshAliases(); await settle(2); }
    expect(peak).toBeLessThanOrEqual(1);
    __setBackendForTest(null);
  });

  // R10-06: the watcher's reopen (graph-rebound → applyGraphReopened,
  // graph.ts:935-938) bumps binding + epoch only. The navigation index is
  // re-read only from the dataRev / pageInventoryRev effects (App.tsx:1387,
  // 1390) or a request already in flight (reaskAfterRepaint, which also gives
  // up because the binding moved). So after a `:hidden` reopen the alias map
  // and page identities stay the old Graph's until the next save — the R9-13
  // symptom, for the navigation half.
  it("re-reads the navigation index after the backend reopens the graph", async () => {
    const api = mockBackend();
    const PAGES = [
      { name: "Alpha", kind: "page", path: "pages/Alpha.md" },
      { name: "Beta", kind: "page", path: "hidden/Beta.md" },
    ];
    const aliases = vi.spyOn(api, "pageAliases").mockResolvedValue([["Gamma", "Beta"]] as never);
    const list = vi.spyOn(api, "listPages").mockResolvedValue(PAGES as never);
    __setBackendForTest(api);
    const { applyGraphReopened, loadNavigationIndex } = await import("./graph");
    const { resolveAlias } = await import("./ui");
    bumpGraphEpoch();
    await loadNavigationIndex();
    expect(resolveAlias("gamma")).toBe("Beta");
    // `:hidden ["hidden"]` lands; the reopened Graph no longer has Beta.
    aliases.mockResolvedValue([] as never);
    list.mockResolvedValue([PAGES[0]] as never);
    const before = list.mock.calls.length + aliases.mock.calls.length;
    applyGraphReopened();
    await settle();
    expect(list.mock.calls.length + aliases.mock.calls.length).toBeGreaterThan(before);
    __setBackendForTest(null);
  });

  // R10-10: every reader of the graph's page list goes through
  // `listGraphPages` (src/pageList.ts), which asks the backend once per
  // binding, epoch and inventory revision. A second direct `listPages()` call
  // is a second producer, and each one is a whole-graph IPC.
  it("asks the backend for the page list in one place", () => {
    const walk = (dir: string): string[] => readdirSync(dir).flatMap((name) => {
      const path = join(dir, name);
      if (statSync(path).isDirectory()) return walk(path);
      return /\.tsx?$/.test(name) && !/\.test\.tsx?$/.test(name) ? [path] : [];
    });
    const callers = walk("src").filter((path) => /\.listPages\(\)/.test(readFileSync(path, "utf8")));
    expect(callers, "list pages through listGraphPages (src/pageList.ts), GH #543 R10-10").toEqual([join("src", "pageList.ts")]);
  });

  // Launch design D4 (GH #550): answers shown during the launch check may come
  // from the index as the last session left it, so the check's completion
  // event must reach `correctLaunchAnswers`, which re-asks every surface. The
  // surfaces' own re-ask is tested in LinkedReferences.test.tsx and
  // QuickSwitcher.test.tsx; this pins that the app wires the event to it.
  it("corrects launch answers when the launch check lands", async () => {
    const app = readFileSync(join("src", "App.tsx"), "utf8");
    expect(app, "App.tsx must call correctLaunchAnswers on warm-cache-done (launch design D4)")
      .toContain('listenHere("warm-cache-done", () => correctLaunchAnswers())');
    const ui = await import("./ui");
    const before = [ui.indexCorrectionRev(), ui.pageInventoryRev(), ui.dataRev()];
    ui.correctLaunchAnswers();
    expect([ui.indexCorrectionRev(), ui.pageInventoryRev(), ui.dataRev()]).toEqual(before.map((n) => n + 1));
  });
});
